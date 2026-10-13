//! # Recording Module - streaming transcription
//!
//! Records audio from PulseAudio, runs voice activity detection, and produces a
//! live transcript in two parts:
//!
//! - **confirmed** text (stable, shown in white), and
//! - **preview** text (tentative, shown in grey).
//!
//! ## Algorithm
//!
//! While a session is active, audio chunks are appended to a buffer. Every
//! `periodic-transcription-interval` seconds the unconfirmed tail is decoded
//! once, with the confirmed text passed as the decoder's initial prompt.
//!
//! The decode covers `[confirmed .. confirmed + transcription-window]`. The
//! window is kept under Whisper's 30 s limit so one pass is enough. Segments
//! that end older than `transcription-lag` are committed to the confirmed text,
//! which advances `confirmed` to that segment boundary. The remaining segments
//! become the preview. Because commits fall on Whisper segment boundaries, a
//! word is never cut in half between the confirmed and preview parts.
//!
//! ```mermaid
//! graph LR
//!   A["confirmed"] --> B["settled (older than lag)"] --> C["preview (recent)"]
//! ```
//!
//! ## Behaviour
//!
//! - A pre-roll buffer keeps a short slice of pre-onset audio, so soft onsets
//!   are not clipped.
//! - Only one transcription runs at a time; a tick is skipped while one is
//!   queued or running.
//! - At the end of the session any remaining unconfirmed audio is decoded and
//!   appended, so every chunk is transcribed exactly once.

use anyhow::Result;
use async_channel::Sender;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use crate::audio::{AudioChunk, AudioRecorder, RecordingCommand, VadState, VoiceActivityDetector};
use crate::config::Config;
use crate::defaults;
use crate::dialog::TranscriptionState;
use crate::pulse::PulseManager;
use crate::source_mute::SourceMuteManager;
use crate::transcription::{TranscribedSegment, Transcriber};

/// UIMessage types for communication with the daemon app
/// This is re-exported from daemon_app module
#[derive(Debug, Clone)]
pub enum UIMessage {
    UpdateState(TranscriptionState, String, f64),
    SetText(String),
    SetTextPreview(String),
    /// Set both confirmed (white) and preview (grey) text
    /// (confirmed_text, preview_text)
    SetConfirmedAndPreview(String, String),
    AutoSendText(String, Option<usize>), // Auto-send text (text, slot); None = default target
    SetDestinations(Vec<crate::dialog::DestinationSlot>), // Update destination slots
    StoreHandlerWindowIds(Vec<u64>),     // Store window IDs managed by handler
    /// Set the microphone description shown in the UI. Looked up off the
    /// critical path so capture can start immediately.
    SetSourceInfo(String),
    CloseImmediately, // Close dialog immediately (for stop-and-send, cleanup happens later)
    Close,
}

/// Shared services and state needed to run one recording session.
pub struct RecordingSession {
    pub config: Arc<Config>,
    pub transcriber: Arc<Transcriber>,
    pub pulse: Arc<PulseManager>,
    pub ui_tx: Sender<UIMessage>,
    pub stop_tx_storage: Arc<Mutex<Option<std::sync::mpsc::Sender<RecordingCommand>>>>,
    pub auto_send_slots: Arc<Mutex<Vec<Option<usize>>>>,
}

/// RAII guard that clears the pipeline's pending flag when dropped, including
/// on panic, so a failed worker cannot wedge future transcriptions.
struct PendingGuard(Arc<AtomicBool>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Shared state and services for periodic transcription.
#[derive(Clone)]
struct TranscriptionPipeline {
    transcriber: Arc<Transcriber>,
    ui_tx: Sender<UIMessage>,
    confirmed_text: Arc<Mutex<String>>,
    confirmed_chunks: Arc<Mutex<usize>>,
    /// Held for the duration of a worker, so session teardown can wait for it.
    in_flight: Arc<Mutex<()>>,
    /// Set while a worker is queued or running, so workers cannot stack up.
    pending: Arc<AtomicBool>,
    chunk_size: usize,
}

impl TranscriptionPipeline {
    /// Claim the single transcription slot. Returns `false` when one is already
    /// queued or running.
    fn try_begin(&self) -> bool {
        !self.pending.swap(true, Ordering::AcqRel)
    }

    /// Whether a worker is queued or running.
    fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    /// Decode `audio` (starting at chunk `first_chunk`) on a worker thread.
    ///
    /// Segments ending at or before `settled` are committed to the confirmed
    /// text, so cuts fall on Whisper segment boundaries rather than arbitrary
    /// sample positions. The remaining segments become the preview.
    fn spawn(&self, audio: Vec<f32>, first_chunk: usize, settled: usize, available_end: usize) {
        let pipeline = self.clone();
        thread::spawn(move || pipeline.run(&audio, first_chunk, settled, available_end));
    }

    fn run(&self, audio: &[f32], first_chunk: usize, settled: usize, available_end: usize) {
        let _in_flight = self.in_flight.lock();
        let _pending = PendingGuard(Arc::clone(&self.pending));

        if audio.is_empty() {
            return;
        }

        let prompt = {
            let confirmed = self.confirmed_text.lock();
            tail_prompt(&confirmed)
        };

        let segments = match self
            .transcriber
            .transcribe_segments(audio, Some(&prompt), true)
        {
            Ok(segments) => segments,
            Err(e) => {
                tracing::warn!(error = %e, "Periodic transcription failed");
                return;
            }
        };

        let (new_confirmed, committed, preview) = split_by_settled(
            &segments,
            first_chunk,
            settled,
            available_end,
            self.chunk_size,
        );

        if new_confirmed > first_chunk {
            append_confirmed(&self.confirmed_text, &committed);
            *self.confirmed_chunks.lock() = new_confirmed;
        }

        let confirmed = self.confirmed_text.lock().clone();
        let _ = self.ui_tx.send_blocking(UIMessage::SetConfirmedAndPreview(
            confirmed,
            preview.trim().to_string(),
        ));
    }
}

/// Start a transcription of the unconfirmed tail, if one is not already
/// running. Returns whether a worker was spawned.
fn trigger_transcription(
    recorded_audio: &[Vec<f32>],
    pipeline: &TranscriptionPipeline,
    confirmed_chunks: &Mutex<usize>,
    lag_chunks: usize,
    max_tail_chunks: usize,
) -> bool {
    if recorded_audio.is_empty() {
        return false;
    }

    let total_chunks = recorded_audio.len();
    let current_confirmed = *confirmed_chunks.lock();
    let tail_end = total_chunks.min(current_confirmed + max_tail_chunks);
    if tail_end <= current_confirmed || !pipeline.try_begin() {
        return false;
    }

    let settled_chunks = total_chunks.saturating_sub(lag_chunks);
    let tail_audio: Vec<f32> = recorded_audio[current_confirmed..tail_end]
        .iter()
        .flatten()
        .copied()
        .collect();

    tracing::debug!(
        total_chunks,
        settled_chunks,
        current_confirmed,
        tail_chunks = tail_end - current_confirmed,
        "Periodic transcription"
    );

    pipeline.spawn(tail_audio, current_confirmed, settled_chunks, tail_end);
    true
}

/// Append `addition` to the confirmed text, separated by a single space.
fn append_confirmed(target: &Mutex<String>, addition: &str) {
    let addition = addition.trim();
    if addition.is_empty() {
        return;
    }
    let mut text = target.lock();
    if !text.is_empty() && !text.ends_with(' ') {
        text.push(' ');
    }
    text.push_str(addition);
}

/// Convert a Whisper segment end time (centiseconds of 16 kHz audio) to chunks.
fn samples_to_chunks(centiseconds: i64, chunk_size: usize) -> usize {
    let samples = (centiseconds.max(0) as u64).saturating_mul(160);
    (samples / chunk_size.max(1) as u64) as usize
}

/// Split decoded segments into (new confirmed chunk index, committed text,
/// preview text).
///
/// A segment is committed when it ends at or before `settled` (the audio older
/// than the confirmation lag) and within `available_end` (the end of the audio
/// that was decoded). Everything else is preview.
fn split_by_settled(
    segments: &[TranscribedSegment],
    first_chunk: usize,
    settled: usize,
    available_end: usize,
    chunk_size: usize,
) -> (usize, String, String) {
    let mut new_confirmed = first_chunk;
    let mut committed = String::new();
    let mut preview = String::new();
    for segment in segments {
        let end_chunk = first_chunk + samples_to_chunks(segment.end_cs, chunk_size);
        if end_chunk <= settled && end_chunk <= available_end {
            committed.push_str(&segment.text);
            new_confirmed = new_confirmed.max(end_chunk);
        } else {
            preview.push_str(&segment.text);
        }
    }
    (new_confirmed, committed, preview)
}

/// The tail of `text` used as a decoding prompt, cut on a character boundary.
fn tail_prompt(text: &str) -> String {
    const MAX_PROMPT_CHARS: usize = 224;
    if text.len() <= MAX_PROMPT_CHARS {
        return text.to_owned();
    }
    let mut start = text.len() - MAX_PROMPT_CHARS;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

pub fn start_recording_session(
    session: RecordingSession,
    unmute_source: bool,
    source: Option<String>,
) -> Result<()> {
    let RecordingSession {
        config,
        transcriber,
        pulse,
        ui_tx,
        stop_tx_storage,
        auto_send_slots,
    } = session;

    tracing::info!("Starting recording session");

    // Unmute source if requested
    let _source_manager = if unmute_source {
        match SourceMuteManager::unmute_if_needed(Arc::clone(&pulse)) {
            Ok(manager) => Some(manager),
            Err(e) => {
                tracing::warn!(
                    "Failed to manage source mute state: {}. Continuing anyway.",
                    e
                );
                None
            }
        }
    } else {
        None
    };

    // Create audio recorder with optional source selection.
    // No PulseAudio introspection here: capture must start as fast as possible.
    // The microphone description shown in the UI is looked up on a background
    // thread by the daemon, not on this path.
    let recorder = AudioRecorder::new(16000, source, config.chunk_size)?;

    // PulseAudio resamples to the requested rate, so this is what we get.
    let actual_sample_rate = recorder.get_actual_sample_rate()?;

    // Channels for audio data
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<AudioChunk>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<RecordingCommand>();
    let stop_rx = Arc::new(Mutex::new(stop_rx));

    // Set by the audio thread if reading fails (e.g. the audio server went
    // away), as opposed to a requested stop.
    let stream_error = Arc::new(AtomicBool::new(false));

    // Store stop_tx BEFORE starting recording so Escape key can use it
    *stop_tx_storage.lock() = Some(stop_tx);

    // Start audio stream (runs in background thread)
    recorder.start_recording(chunk_tx, stop_rx, Arc::clone(&stream_error))?;

    // Use VAD to detect speech start and silence
    // IMPORTANT: Use the ACTUAL sample rate from the device, not the requested rate
    // silence_duration is not used for stopping, only for detecting silence state
    // stop_silence_duration (config.silence_duration) is used to actually stop the session
    tracing::info!(
        silence_threshold = config.silence_threshold,
        min_speech_duration = config.min_speech_duration,
        stop_silence_duration = config.silence_duration,
        sample_rate = actual_sample_rate,
        chunk_size = config.chunk_size,
        periodic_transcription_interval = config.periodic_transcription_interval,
        transcription_window = config.transcription_window,
        transcription_lag = config.transcription_lag,
        "VAD initialized"
    );
    let vad = VoiceActivityDetector::new(
        config.silence_threshold,
        config.min_speech_duration,
        config.silence_duration, // Silence duration to trigger stop
        actual_sample_rate,      // Use ACTUAL sample rate from device
        config.chunk_size,
    );

    let mut recorded_audio: Vec<Vec<f32>> = Vec::new();
    let mut vad_state = VadState::Idle;
    let mut previous_vad_state;
    let mut silence_chunks = 0;
    let mut speech_chunks = 0;
    let mut transcription_started = false;

    // Calculate periodic transcription threshold in chunks
    let periodic_chunks_threshold = (config.periodic_transcription_interval
        * actual_sample_rate as f32
        / config.chunk_size as f32) as usize;
    let mut chunks_since_last_transcription = 0usize;

    // Maximum audio tail decoded in one pass, in chunks. Kept under Whisper's
    // 30 s window so a pass never needs a second encoder seek.
    let max_tail_chunks = (config.transcription_window * actual_sample_rate as f32
        / config.chunk_size as f32) as usize;

    // Calculate lag in chunks (audio held back before eligible for confirmation)
    let lag_chunks =
        (config.transcription_lag * actual_sample_rate as f32 / config.chunk_size as f32) as usize;

    // Pre-roll: keep the most recent few chunks of audio so that a soft speech
    // onset (a plosive or fricative sits below the RMS threshold) is not
    // clipped when the VAD first fires. This only buffers audio captured during
    // this session; no stream is held between sessions.
    let pre_roll_chunks = if defaults::PRE_ROLL_DURATION > 0.0 {
        ((defaults::PRE_ROLL_DURATION * actual_sample_rate as f32 / config.chunk_size as f32).ceil()
            as usize)
            .max(1)
    } else {
        0
    };
    let mut pre_roll: VecDeque<Vec<f32>> = VecDeque::with_capacity(pre_roll_chunks + 1);

    // Track confirmed text and how many chunks have been confirmed
    let confirmed_text = Arc::new(Mutex::new(String::new()));
    let confirmed_chunks = Arc::new(Mutex::new(0usize));

    // Held for the duration of a transcription worker so session teardown can
    // wait for it to finish.
    let transcription_in_flight = Arc::new(Mutex::new(()));

    let pipeline = TranscriptionPipeline {
        transcriber: Arc::clone(&transcriber),
        ui_tx: ui_tx.clone(),
        confirmed_text: Arc::clone(&confirmed_text),
        confirmed_chunks: Arc::clone(&confirmed_chunks),
        in_flight: Arc::clone(&transcription_in_flight),
        pending: Arc::new(AtomicBool::new(false)),
        chunk_size: config.chunk_size,
    };

    // Start in Recording state (waiting for speech)
    let _ = ui_tx.send_blocking(UIMessage::UpdateState(
        TranscriptionState::Recording,
        "Listening...".to_string(),
        0.0,
    ));

    tracing::info!("Waiting for speech to begin...");

    loop {
        match chunk_rx.recv_timeout(defaults::CHUNK_RECV_TIMEOUT) {
            Ok(chunk) => {
                let rms = chunk.rms;
                tracing::trace!("Received audio chunk: RMS = {}", rms);

                // Process through VAD
                let result = vad.process_chunk(rms, vad_state, silence_chunks, speech_chunks);
                tracing::trace!(
                    "VAD: state transition {:?} -> {:?}, silence_chunks={}, should_stop={}",
                    vad_state,
                    result.state,
                    silence_chunks,
                    result.should_stop
                );
                previous_vad_state = vad_state;
                vad_state = result.state;

                // Detect transition from Speaking to SilenceAfterSpeech
                let transitioned_to_silence = previous_vad_state == VadState::Speaking
                    && vad_state == VadState::SilenceAfterSpeech;

                // Bound memory for long sessions: audio at or before the
                // confirmed boundary is never decoded again, so drop it. Only
                // when no worker is running, so the boundary cannot move under
                // the trim.
                if !pipeline.is_pending() {
                    let confirmed = *confirmed_chunks.lock();
                    if confirmed >= lag_chunks.max(8) {
                        recorded_audio.drain(0..confirmed);
                        *confirmed_chunks.lock() = 0;
                        tracing::trace!(dropped = confirmed, "Trimmed committed audio");
                    }
                }

                use VadState::*;
                match vad_state {
                    Speaking => {
                        silence_chunks = 0;
                        speech_chunks += 1;
                        chunks_since_last_transcription += 1;

                        // On the first speech chunk, prepend the buffered
                        // pre-onset audio so the start of the utterance is kept.
                        if previous_vad_state == Idle && !pre_roll.is_empty() {
                            let buffered = pre_roll.len();
                            recorded_audio.extend(pre_roll.drain(..));
                            tracing::debug!(buffered, "Prepended pre-roll audio at speech onset");
                        }

                        recorded_audio.push(chunk.data);

                        // Switch to transcription mode once speech starts
                        if !transcription_started {
                            transcription_started = true;
                            tracing::info!(
                                "Speech detected! Transcription will run periodically..."
                            );
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Processing,
                                "Recording...".to_string(),
                                rms as f64,
                            ));
                        } else {
                            // Update UI with audio level
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Processing,
                                "Recording...".to_string(),
                                rms as f64,
                            ));
                        }

                        // Periodic transcription while speaking
                        if chunks_since_last_transcription >= periodic_chunks_threshold
                            && trigger_transcription(
                                &recorded_audio,
                                &pipeline,
                                &confirmed_chunks,
                                lag_chunks,
                                max_tail_chunks,
                            )
                        {
                            chunks_since_last_transcription = 0;
                        }
                    }
                    SilenceAfterSpeech => {
                        silence_chunks += 1;
                        chunks_since_last_transcription += 1;
                        recorded_audio.push(chunk.data);

                        // Update UI with audio level
                        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                            TranscriptionState::Processing,
                            "Transcribing...".to_string(),
                            rms as f64,
                        ));

                        // Trigger periodic transcription even during silence
                        // (VAD might be wrong, user might still be speaking quietly)
                        let should_periodic_transcribe =
                            chunks_since_last_transcription >= periodic_chunks_threshold;

                        if (transitioned_to_silence || should_periodic_transcribe)
                            && trigger_transcription(
                                &recorded_audio,
                                &pipeline,
                                &confirmed_chunks,
                                lag_chunks,
                                max_tail_chunks,
                            )
                        {
                            chunks_since_last_transcription = 0;
                        }

                        if result.should_stop {
                            tracing::debug!(
                                "VAD detected end of speech (silence duration exceeded)"
                            );
                            break;
                        }
                    }
                    Idle => {
                        // Keep recent audio so a soft speech onset is not lost.
                        if pre_roll_chunks > 0 {
                            pre_roll.push_back(chunk.data);
                            while pre_roll.len() > pre_roll_chunks {
                                pre_roll.pop_front();
                            }
                        }

                        // Still waiting for speech
                        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                            TranscriptionState::Recording,
                            "Listening...".to_string(),
                            rms as f64,
                        ));
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                tracing::info!("Audio stream ended");
                break;
            }
        }
    }

    // Audio stream cleanup happens automatically in the background thread

    // Wait for any in-flight transcription to complete before reading final values
    // Hold the lock while reading to ensure consistency
    let _transcription_guard = transcription_in_flight.lock();

    // A read failure means the audio server went away (for example a
    // PulseAudio or PipeWire restart) rather than a requested stop. End the
    // session cleanly with an error instead of reporting "no speech detected".
    if stream_error.load(Ordering::Acquire) {
        tracing::warn!("Audio source disconnected; ending session");
        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
            TranscriptionState::Error,
            "Audio source disconnected".to_string(),
            0.0,
        ));
        thread::sleep(defaults::ERROR_DISPLAY_DURATION);
        let _ = ui_tx.send_blocking(UIMessage::Close);
        return Ok(());
    }

    // Get confirmed text so far (safe - we hold the transcription lock)
    let mut final_text = confirmed_text.lock().clone();
    let current_confirmed_idx = *confirmed_chunks.lock();

    tracing::debug!(
        confirmed_text = %final_text,
        confirmed_chunks = current_confirmed_idx,
        total_chunks = recorded_audio.len(),
        "Final transcription - processing remaining audio"
    );

    // Check if we should auto-send to specific slots (atomically retrieve and clear)
    let auto_slots = {
        let mut slots = auto_send_slots.lock();
        let result = slots.clone();
        slots.clear(); // Clear for next session
        result
    };
    tracing::debug!(auto_slots = ?auto_slots, "Auto-send slots");

    // Transcribe any remaining unconfirmed audio
    if current_confirmed_idx < recorded_audio.len() {
        let remaining_audio: Vec<f32> = recorded_audio[current_confirmed_idx..]
            .iter()
            .flatten()
            .copied()
            .collect();

        tracing::debug!(
            remaining_chunks = recorded_audio.len() - current_confirmed_idx,
            "Transcribing remaining unconfirmed audio"
        );

        let prompt = tail_prompt(&final_text);
        match transcriber.transcribe_segments(&remaining_audio, Some(&prompt), false) {
            Ok(segments) => {
                let text: String = segments
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .collect();
                let cleaned = text.trim();
                if !cleaned.is_empty() {
                    if !final_text.is_empty() {
                        final_text.push(' ');
                    }
                    final_text.push_str(cleaned);
                    tracing::debug!(final_text = %final_text, "Added remaining audio to final text");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to transcribe remaining audio");
            }
        }
    }

    // If still no text and we have audio, do a full transcription as fallback
    if final_text.is_empty() && !recorded_audio.is_empty() {
        tracing::info!(
            chunks = recorded_audio.len(),
            "No text from windowed approach, trying full transcription"
        );
        let audio_flat: Vec<f32> = recorded_audio.into_iter().flatten().collect();

        match transcriber.transcribe(&audio_flat) {
            Ok(result) => {
                let cleaned = result.trim().to_string();
                tracing::debug!(result = %cleaned, "Full transcription result");
                if !cleaned.is_empty() {
                    final_text = cleaned;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Full transcription failed");
            }
        }
    }

    if final_text.is_empty() {
        // No text at all - show error
        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
            TranscriptionState::Error,
            "No speech detected".to_string(),
            0.0,
        ));
        thread::sleep(defaults::ERROR_DISPLAY_DURATION);
        let _ = ui_tx.send_blocking(UIMessage::Close);
        return Ok(());
    }

    // Auto-send or show final result in dialog for review
    if !auto_slots.is_empty() {
        // Send to all requested slots
        for slot in &auto_slots {
            tracing::info!(
                slot,
                text = %final_text,
                "Auto-sending to slot"
            );
            let _ = ui_tx.send_blocking(UIMessage::AutoSendText(final_text.clone(), *slot));
        }
        tracing::info!(slots = auto_slots.len(), "AutoSendText messages sent");
        // Send Close message after all AutoSendText messages to cleanup and hide dialog
        let _ = ui_tx.send_blocking(UIMessage::Close);
    } else {
        tracing::debug!(text = %final_text, "Showing text for review");
        let _ = ui_tx.send_blocking(UIMessage::SetText(final_text));
        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
            TranscriptionState::Reviewing,
            "Ready to send".to_string(),
            0.0,
        ));
    }

    Ok(())
}

pub fn execute_output_command(
    command_template: &str,
    text: &str,
    slot_num: usize,
    extra_env_vars: &[(String, String)],
) {
    tracing::info!(
        transcription = text,
        slot_num,
        command_template,
        "Executing command"
    );

    // Pass transcription via environment variable to prevent shell injection
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command_template)
        .env("TRANSCRIPTION", text);

    // Add handler-specific environment variables
    for (key, value) in extra_env_vars {
        cmd.env(key, value);
    }

    match cmd.status() {
        Ok(status) => {
            if !status.success() {
                tracing::error!(
                    status = ?status,
                    "Command failed"
                );
            } else {
                tracing::info!("Command executed successfully");
            }
        }
        Err(e) => {
            tracing::error!(error = %e, "Failed to execute command");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_samples_to_chunks() {
        // 16 kHz: 160 samples per centisecond. 100 cs = 1 s = 16000 samples.
        assert_eq!(samples_to_chunks(0, 1024), 0);
        assert_eq!(samples_to_chunks(100, 1024), 16_000 / 1_024);
        assert_eq!(samples_to_chunks(-5, 1024), 0);
        // chunk_size is guarded against zero.
        assert_eq!(samples_to_chunks(64, 0), 10_240);
    }

    #[test]
    fn test_tail_prompt_short() {
        assert_eq!(tail_prompt("hello"), "hello");
    }

    #[test]
    fn test_tail_prompt_truncates_on_char_boundary() {
        let text = "aé".repeat(300);
        let prompt = tail_prompt(&text);
        assert!(text.ends_with(&prompt));
        assert!(prompt.len() <= 225, "prompt len {}", prompt.len());
    }

    #[test]
    fn test_split_by_settled() {
        // 32 cs -> 5120 samples -> 5 chunks; 640 cs -> 102400 samples -> 100 chunks.
        let segments = vec![
            TranscribedSegment {
                end_cs: 32,
                text: " hello".to_string(),
            },
            TranscribedSegment {
                end_cs: 640,
                text: " world".to_string(),
            },
        ];
        let (new_confirmed, committed, preview) = split_by_settled(&segments, 0, 50, 200, 1024);
        assert_eq!(new_confirmed, 5, "first segment is settled");
        assert_eq!(committed, " hello");
        assert_eq!(preview, " world", "second segment is not settled");
    }

    #[test]
    fn test_split_by_settled_respects_available_end() {
        let segments = vec![TranscribedSegment {
            end_cs: 640,
            text: " text".to_string(),
        }];
        // Older than settled, but beyond the audio that was decoded.
        let (new_confirmed, committed, preview) = split_by_settled(&segments, 0, 1_000, 10, 1024);
        assert_eq!(new_confirmed, 0);
        assert!(committed.is_empty());
        assert_eq!(preview, " text");
    }
}
