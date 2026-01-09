//! # Recording Module - Sliding Window Transcription Algorithm
//!
//! This module handles audio recording and real-time transcription using a sliding
//! window approach that provides stable "confirmed" text and tentative "preview" text.
//!
//! ## Algorithm Overview
//!
//! The transcription uses three key parameters:
//! - **window_chunks**: Size of the confirmation window (e.g., 5 seconds)
//! - **lag_chunks**: How far behind we confirm audio (e.g., 1 second)
//! - **periodic_threshold**: How often we run transcription (e.g., every 1 second)
//!
//! ```text
//! Audio Timeline (chunks arriving over time):
//!
//! |<-------- confirmed -------->|<-- lag -->|
//! [0][1][2][3][4][5][6][7][8][9][10][11][12][13][14]  ← total chunks
//!                                ^
//!                                └── current time (chunk 14 just arrived)
//!
//! settled_chunks = total - lag = 14 - 3 = 11
//! can_confirm = settled_chunks >= confirmed_idx + window_size
//! ```
//!
//! ## Sliding Window State Machine
//!
//! ```text
//!                    ┌─────────────────────────────────────┐
//!                    │         Recording Session           │
//!                    └─────────────────────────────────────┘
//!                                    │
//!                                    ▼
//!                    ┌─────────────────────────────────────┐
//!                    │  Initialize: confirmed_idx = 0      │
//!                    │  window_size, lag, periodic set     │
//!                    └─────────────────────────────────────┘
//!                                    │
//!                                    ▼
//!          ┌─────────────────────────────────────────────────────┐
//!          │                  For each chunk:                    │
//!          │  1. Add chunk to recorded_audio                     │
//!          │  2. Increment chunks_since_transcription            │
//!          └─────────────────────────────────────────────────────┘
//!                                    │
//!                ┌───────────────────┴───────────────────┐
//!                │ chunks_since_transcription            │
//!                │      >= periodic_threshold?           │
//!                └───────────────────┬───────────────────┘
//!                       No │                  │ Yes
//!                          │                  ▼
//!                          │    ┌──────────────────────────┐
//!                          │    │ settled = total - lag    │
//!                          │    │ can_confirm =            │
//!                          │    │   settled >= confirmed   │
//!                          │    │            + window      │
//!                          │    └──────────────────────────┘
//!                          │                  │
//!                          │         ┌───────┴───────┐
//!                          │         │ can_confirm?  │
//!                          │         └───────┬───────┘
//!                          │       No │           │ Yes
//!                          │          │           ▼
//!                          │          │  ┌────────────────────────┐
//!                          │          │  │ Transcribe window:     │
//!                          │          │  │ [confirmed..confirmed  │
//!                          │          │  │          +window]      │
//!                          │          │  │ confirmed += window    │
//!                          │          │  └────────────────────────┘
//!                          │          │           │
//!                          │          └─────┬─────┘
//!                          │                ▼
//!                          │    ┌──────────────────────────┐
//!                          │    │ Transcribe preview:      │
//!                          │    │ [confirmed..total]       │
//!                          │    │ (tentative, may change)  │
//!                          │    └──────────────────────────┘
//!                          │                │
//!                          └────────────────┘
//!                                    │
//!                                    ▼
//!                          ┌─────────────────┐
//!                          │  Session ends   │
//!                          │  (silence/stop) │
//!                          └─────────────────┘
//!                                    │
//!                                    ▼
//!                    ┌─────────────────────────────────────┐
//!                    │  Final: Transcribe remaining        │
//!                    │  [confirmed..total] as confirmed    │
//!                    └─────────────────────────────────────┘
//! ```
//!
//! ## Example Timeline
//!
//! ```text
//! Config: window=10, lag=3, periodic=5
//!
//! Time →
//! Chunks:  [0][1][2][3][4][5][6][7][8][9][10][11][12][13][14][15][16][17][18]
//!
//! At chunk 5 (periodic trigger):
//!   total=5, settled=2, confirmed=0, can_confirm=false (2 < 0+10)
//!   Preview: [0..5]
//!
//! At chunk 10:
//!   total=10, settled=7, confirmed=0, can_confirm=false (7 < 0+10)
//!   Preview: [0..10]
//!
//! At chunk 15:
//!   total=15, settled=12, confirmed=0, can_confirm=true (12 >= 0+10)
//!   Confirm: [0..10] → confirmed=10
//!   Preview: [10..15]
//!
//! At end (chunk 18):
//!   Remaining: [10..18] transcribed as final
//!
//! Result: All chunks [0..18] transcribed exactly once
//! ```
//!
//! ## Guarantees
//!
//! The algorithm guarantees:
//! 1. **No gaps**: Every chunk is eventually transcribed
//! 2. **No overlaps**: No chunk is transcribed twice in confirmed output
//! 3. **Contiguous windows**: Each confirmed window starts where the previous ended
//! 4. **Complete coverage**: Remaining chunks at end are always transcribed

use anyhow::Result;
use async_channel::Sender;
use parking_lot::Mutex;
use std::sync::Arc;
use std::thread;

use crate::audio::{AudioChunk, AudioRecorder, RecordingCommand, VadState, VoiceActivityDetector};
use crate::config::Config;
use crate::constants::{CHUNK_RECV_TIMEOUT, ERROR_DISPLAY_DURATION};
use crate::dialog::TranscriptionState;
use crate::source_mute::SourceMuteManager;
use crate::transcription::Transcriber;

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
    AutoSendText(String, usize), // Auto-send text to specified slot (text, slot_num)
    SetDestinations(Vec<crate::dialog::DestinationSlot>), // Update destination slots
    StoreHandlerWindowIds(Vec<u64>), // Store window IDs managed by handler
    CloseImmediately, // Close dialog immediately (for stop-and-send, cleanup happens later)
    Close,
}

pub fn start_recording_session(
    config: Arc<Config>,
    transcriber: Arc<Transcriber>,
    ui_tx: Sender<UIMessage>,
    stop_tx_storage: Arc<Mutex<Option<std::sync::mpsc::Sender<RecordingCommand>>>>,
    unmute_source: bool,
    source: Option<String>,
    auto_send_slots: Arc<Mutex<Vec<usize>>>,
) -> Result<()> {
    tracing::info!("Starting recording session");

    // Unmute source if requested
    let _source_manager = if unmute_source {
        match SourceMuteManager::unmute_if_needed() {
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

    // Create audio recorder with optional source selection
    let recorder = AudioRecorder::new(16000, source, config.chunk_size)?;

    // Log which microphone is being used
    let device_name = recorder
        .get_device_name()
        .unwrap_or_else(|_| "Unknown".to_string());
    tracing::info!(microphone = %device_name, "Recording with microphone: {device_name}");

    // Get the ACTUAL sample rate the device is using (not what we requested)
    let actual_sample_rate = recorder.get_actual_sample_rate()?;

    // Channels for audio data
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<AudioChunk>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<RecordingCommand>();
    let stop_rx = Arc::new(Mutex::new(stop_rx));

    // Store stop_tx BEFORE starting recording so Escape key can use it
    *stop_tx_storage.lock() = Some(stop_tx);

    // Start audio stream (runs in background thread)
    recorder.start_recording(chunk_tx, stop_rx)?;

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

    // Calculate transcription window size in chunks (for sliding window)
    let window_chunks = (config.transcription_window * actual_sample_rate as f32
        / config.chunk_size as f32) as usize;

    // Calculate lag in chunks (audio held back before eligible for confirmation)
    let lag_chunks =
        (config.transcription_lag * actual_sample_rate as f32 / config.chunk_size as f32) as usize;

    // Track confirmed text and how many chunks have been confirmed
    let confirmed_text = Arc::new(Mutex::new(String::new()));
    let confirmed_chunks = Arc::new(Mutex::new(0usize));

    // Track if a transcription is currently in flight to prevent concurrent transcriptions
    // Using Mutex for RAII - holding the lock means transcription is in flight
    let transcription_in_flight = Arc::new(Mutex::new(()));

    let transcriber_clone = Arc::clone(&transcriber);
    let ui_tx_for_transcription = ui_tx.clone();
    let confirmed_text_clone = Arc::clone(&confirmed_text);
    let confirmed_chunks_clone = Arc::clone(&confirmed_chunks);

    // Start in Recording state (waiting for speech)
    let _ = ui_tx.send_blocking(UIMessage::UpdateState(
        TranscriptionState::Recording,
        "Listening...".to_string(),
        0.0,
    ));

    tracing::info!("Waiting for speech to begin...");

    loop {
        match chunk_rx.recv_timeout(CHUNK_RECV_TIMEOUT) {
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

                use VadState::*;
                match vad_state {
                    Speaking => {
                        silence_chunks = 0;
                        speech_chunks += 1;
                        chunks_since_last_transcription += 1;
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
                            && !recorded_audio.is_empty()
                            && let Some(_guard) = transcription_in_flight.try_lock()
                        {
                            chunks_since_last_transcription = 0;

                            let total_chunks = recorded_audio.len();
                            let current_confirmed = *confirmed_chunks.lock();

                            // Calculate settled audio (older than lag)
                            let settled_chunks = total_chunks.saturating_sub(lag_chunks);

                            // Check if we have a full window of settled audio to confirm
                            let can_confirm = settled_chunks >= current_confirmed + window_chunks;

                            // Clone audio for transcription
                            let audio_for_transcription: Vec<Vec<f32>> = recorded_audio.clone();

                            tracing::info!(
                                total_chunks,
                                settled_chunks,
                                current_confirmed,
                                can_confirm,
                                "Periodic transcription"
                            );

                            let transcriber_ref = Arc::clone(&transcriber_clone);
                            let ui_tx_transcribe = ui_tx_for_transcription.clone();
                            let confirmed_text_ref = Arc::clone(&confirmed_text_clone);
                            let confirmed_chunks_ref = Arc::clone(&confirmed_chunks_clone);
                            let in_flight_mutex = Arc::clone(&transcription_in_flight);
                            let window_size = window_chunks;

                            thread::spawn(move || {
                                let _guard = in_flight_mutex.lock();

                                let mut current_confirmed_text = confirmed_text_ref.lock().clone();
                                let mut current_confirmed_idx = *confirmed_chunks_ref.lock();

                                // If we can confirm a window, transcribe it
                                if can_confirm {
                                    let window_end = current_confirmed_idx + window_size;
                                    let window_audio: Vec<f32> = audio_for_transcription
                                        [current_confirmed_idx..window_end]
                                        .iter()
                                        .flatten()
                                        .copied()
                                        .collect();

                                    match transcriber_ref.transcribe(&window_audio) {
                                        Ok(text) => {
                                            let cleaned = text
                                                .replace("[BLANK_AUDIO]", "")
                                                .trim()
                                                .to_string();
                                            if !cleaned.is_empty() {
                                                if !current_confirmed_text.is_empty() {
                                                    current_confirmed_text.push(' ');
                                                }
                                                current_confirmed_text.push_str(&cleaned);
                                                tracing::debug!(
                                                    confirmed = %current_confirmed_text,
                                                    "Window confirmed"
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "Window transcription failed");
                                        }
                                    }

                                    current_confirmed_idx = window_end;

                                    // Update shared state
                                    *confirmed_text_ref.lock() = current_confirmed_text.clone();
                                    *confirmed_chunks_ref.lock() = current_confirmed_idx;
                                }

                                // Always transcribe remaining (unconfirmed) audio as preview
                                let preview_audio: Vec<f32> = audio_for_transcription
                                    [current_confirmed_idx..]
                                    .iter()
                                    .flatten()
                                    .copied()
                                    .collect();

                                let preview_text = if !preview_audio.is_empty() {
                                    match transcriber_ref.transcribe(&preview_audio) {
                                        Ok(text) => {
                                            text.replace("[BLANK_AUDIO]", "").trim().to_string()
                                        }
                                        Err(_) => String::new(),
                                    }
                                } else {
                                    String::new()
                                };

                                // Send confirmed + preview to UI
                                let _ =
                                    ui_tx_transcribe.try_send(UIMessage::SetConfirmedAndPreview(
                                        current_confirmed_text,
                                        preview_text,
                                    ));
                            });
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
                            && !recorded_audio.is_empty()
                        {
                            // Try to acquire the lock (non-blocking)
                            // If we can't acquire it, transcription is already in flight
                            if let Some(_guard) = transcription_in_flight.try_lock() {
                                chunks_since_last_transcription = 0;

                                let total_chunks = recorded_audio.len();
                                let current_confirmed = *confirmed_chunks.lock();

                                // Calculate settled audio (older than lag)
                                let settled_chunks = total_chunks.saturating_sub(lag_chunks);

                                // Check if we have a full window of settled audio to confirm
                                let can_confirm =
                                    settled_chunks >= current_confirmed + window_chunks;

                                // Clone audio for transcription
                                let audio_for_transcription: Vec<Vec<f32>> = recorded_audio.clone();

                                let reason = if transitioned_to_silence {
                                    "pause"
                                } else {
                                    "periodic"
                                };
                                tracing::debug!(
                                    total_chunks,
                                    settled_chunks,
                                    current_confirmed,
                                    can_confirm,
                                    reason,
                                    "Transcribing (silence)"
                                );

                                let transcriber_ref = Arc::clone(&transcriber_clone);
                                let ui_tx_transcribe = ui_tx_for_transcription.clone();
                                let confirmed_text_ref = Arc::clone(&confirmed_text_clone);
                                let confirmed_chunks_ref = Arc::clone(&confirmed_chunks_clone);
                                let in_flight_mutex = Arc::clone(&transcription_in_flight);
                                let window_size = window_chunks;

                                thread::spawn(move || {
                                    let _guard = in_flight_mutex.lock();

                                    let mut current_confirmed_text =
                                        confirmed_text_ref.lock().clone();
                                    let mut current_confirmed_idx = *confirmed_chunks_ref.lock();

                                    // If we can confirm a window, transcribe it
                                    if can_confirm {
                                        let window_end = current_confirmed_idx + window_size;
                                        let window_audio: Vec<f32> = audio_for_transcription
                                            [current_confirmed_idx..window_end]
                                            .iter()
                                            .flatten()
                                            .copied()
                                            .collect();

                                        match transcriber_ref.transcribe(&window_audio) {
                                            Ok(text) => {
                                                let cleaned = text
                                                    .replace("[BLANK_AUDIO]", "")
                                                    .trim()
                                                    .to_string();
                                                if !cleaned.is_empty() {
                                                    if !current_confirmed_text.is_empty() {
                                                        current_confirmed_text.push(' ');
                                                    }
                                                    current_confirmed_text.push_str(&cleaned);
                                                    tracing::debug!(
                                                        confirmed = %current_confirmed_text,
                                                        "Window confirmed"
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                tracing::warn!(error = %e, "Window transcription failed");
                                            }
                                        }

                                        current_confirmed_idx = window_end;

                                        // Update shared state
                                        *confirmed_text_ref.lock() = current_confirmed_text.clone();
                                        *confirmed_chunks_ref.lock() = current_confirmed_idx;
                                    }

                                    // Always transcribe remaining (unconfirmed) audio as preview
                                    let preview_audio: Vec<f32> = audio_for_transcription
                                        [current_confirmed_idx..]
                                        .iter()
                                        .flatten()
                                        .copied()
                                        .collect();

                                    let preview_text = if !preview_audio.is_empty() {
                                        match transcriber_ref.transcribe(&preview_audio) {
                                            Ok(text) => {
                                                text.replace("[BLANK_AUDIO]", "").trim().to_string()
                                            }
                                            Err(_) => String::new(),
                                        }
                                    } else {
                                        String::new()
                                    };

                                    // Send confirmed + preview to UI
                                    let _ = ui_tx_transcribe.try_send(
                                        UIMessage::SetConfirmedAndPreview(
                                            current_confirmed_text,
                                            preview_text,
                                        ),
                                    );
                                });
                            } else {
                                tracing::debug!("Transcription already in flight, skipping");
                            }
                        }

                        if result.should_stop {
                            tracing::info!(
                                "VAD detected end of speech (silence duration exceeded)"
                            );
                            break;
                        }
                    }
                    Idle => {
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
                tracing::info!("Audio stream stopped (user pressed Escape)");
                break;
            }
        }
    }

    // Audio stream cleanup happens automatically in the background thread

    // Wait for any in-flight transcription to complete before reading final values
    // Hold the lock while reading to ensure consistency
    let _transcription_guard = transcription_in_flight.lock();

    // Get confirmed text so far (safe - we hold the transcription lock)
    let mut final_text = confirmed_text.lock().clone();
    let current_confirmed_idx = *confirmed_chunks.lock();

    tracing::info!(
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
    tracing::info!(auto_slots = ?auto_slots, "Auto-send slots");

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

        match transcriber.transcribe(&remaining_audio) {
            Ok(result) => {
                let cleaned = result.replace("[BLANK_AUDIO]", "").trim().to_string();
                if !cleaned.is_empty() {
                    if !final_text.is_empty() {
                        final_text.push(' ');
                    }
                    final_text.push_str(&cleaned);
                    tracing::info!(final_text = %final_text, "Added remaining audio to final text");
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
                let cleaned = result.replace("[BLANK_AUDIO]", "").trim().to_string();
                tracing::info!(result = %cleaned, "Full transcription result");
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
        thread::sleep(ERROR_DISPLAY_DURATION);
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
        tracing::info!(text = %final_text, "Showing text for review");
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
    use std::collections::HashSet;

    /// Simulates the transcription window algorithm to verify chunk coverage.
    ///
    /// This tracks which chunk ranges are transcribed (confirmed) during periodic
    /// transcription, and which are transcribed at the end (remaining).
    struct TranscriptionWindowSimulator {
        window_chunks: usize,
        lag_chunks: usize,
        periodic_threshold: usize,
        /// Ranges of chunks that were transcribed as "confirmed" windows
        confirmed_ranges: Vec<(usize, usize)>,
        /// Range of chunks transcribed at the end as "remaining"
        remaining_range: Option<(usize, usize)>,
    }

    impl TranscriptionWindowSimulator {
        fn new(window_chunks: usize, lag_chunks: usize, periodic_threshold: usize) -> Self {
            Self {
                window_chunks,
                lag_chunks,
                periodic_threshold,
                confirmed_ranges: Vec::new(),
                remaining_range: None,
            }
        }

        /// Simulates the full recording session with given total chunks.
        /// Returns the set of all chunk indices that were transcribed.
        fn simulate(&mut self, total_chunks: usize) -> HashSet<usize> {
            let mut confirmed_idx = 0usize;
            let mut chunks_since_transcription = 0usize;

            // Simulate chunks arriving one at a time
            for current_total in 1..=total_chunks {
                chunks_since_transcription += 1;

                // Check if we should run periodic transcription
                if chunks_since_transcription >= self.periodic_threshold {
                    chunks_since_transcription = 0;

                    let settled_chunks = current_total.saturating_sub(self.lag_chunks);
                    let can_confirm = settled_chunks >= confirmed_idx + self.window_chunks;

                    if can_confirm {
                        let window_end = confirmed_idx + self.window_chunks;
                        self.confirmed_ranges.push((confirmed_idx, window_end));
                        confirmed_idx = window_end;
                    }
                }
            }

            // At the end, transcribe remaining unconfirmed audio
            if confirmed_idx < total_chunks {
                self.remaining_range = Some((confirmed_idx, total_chunks));
            }

            self.get_transcribed_chunks()
        }

        /// Returns all chunk indices that were transcribed
        fn get_transcribed_chunks(&self) -> HashSet<usize> {
            let mut chunks = HashSet::new();

            for (start, end) in &self.confirmed_ranges {
                for i in *start..*end {
                    chunks.insert(i);
                }
            }

            if let Some((start, end)) = self.remaining_range {
                for i in start..end {
                    chunks.insert(i);
                }
            }

            chunks
        }

        /// Returns chunks that were transcribed more than once (overlap detection)
        fn get_overlapping_chunks(&self) -> HashSet<usize> {
            let mut seen = HashSet::new();
            let mut overlaps = HashSet::new();

            for (start, end) in &self.confirmed_ranges {
                for i in *start..*end {
                    if !seen.insert(i) {
                        overlaps.insert(i);
                    }
                }
            }

            if let Some((start, end)) = self.remaining_range {
                for i in start..end {
                    if !seen.insert(i) {
                        overlaps.insert(i);
                    }
                }
            }

            overlaps
        }
    }

    #[test]
    fn test_all_chunks_transcribed_exact_windows() {
        // Scenario: Total chunks divide evenly into windows
        // window=10, lag=2, periodic=5, total=30
        // Expected: 3 windows of 10 chunks each, no remainder
        let mut sim = TranscriptionWindowSimulator::new(10, 2, 5);
        let transcribed = sim.simulate(30);

        // All 30 chunks should be transcribed
        let expected: HashSet<usize> = (0..30).collect();
        assert_eq!(transcribed, expected, "All chunks should be transcribed");

        // No overlaps
        let overlaps = sim.get_overlapping_chunks();
        assert!(overlaps.is_empty(), "No chunks should be transcribed twice");
    }

    #[test]
    fn test_all_chunks_transcribed_with_remainder() {
        // Scenario: Total chunks don't divide evenly
        // window=10, lag=2, periodic=5, total=35
        // Expected: 3 windows + 5 remaining chunks
        let mut sim = TranscriptionWindowSimulator::new(10, 2, 5);
        let transcribed = sim.simulate(35);

        let expected: HashSet<usize> = (0..35).collect();
        assert_eq!(transcribed, expected, "All chunks should be transcribed");

        // Should have remainder
        assert!(
            sim.remaining_range.is_some(),
            "Should have remaining chunks"
        );

        // No overlaps
        let overlaps = sim.get_overlapping_chunks();
        assert!(overlaps.is_empty(), "No chunks should be transcribed twice");
    }

    #[test]
    fn test_all_chunks_transcribed_small_total() {
        // Scenario: Very few chunks, less than one window
        // window=10, lag=2, periodic=5, total=5
        // Expected: All transcribed as "remaining" at the end
        let mut sim = TranscriptionWindowSimulator::new(10, 2, 5);
        let transcribed = sim.simulate(5);

        let expected: HashSet<usize> = (0..5).collect();
        assert_eq!(transcribed, expected, "All chunks should be transcribed");

        // Should have no confirmed windows, only remaining
        assert!(
            sim.confirmed_ranges.is_empty(),
            "Should have no confirmed windows"
        );
        assert_eq!(
            sim.remaining_range,
            Some((0, 5)),
            "All should be in remaining"
        );
    }

    #[test]
    fn test_all_chunks_transcribed_exactly_one_window() {
        // Scenario: Exactly one window worth of settled audio
        // window=10, lag=2, periodic=5, total=12 (10 settled + 2 lag)
        let mut sim = TranscriptionWindowSimulator::new(10, 2, 5);
        let transcribed = sim.simulate(12);

        let expected: HashSet<usize> = (0..12).collect();
        assert_eq!(transcribed, expected, "All chunks should be transcribed");
    }

    #[test]
    fn test_no_gaps_large_simulation() {
        // Simulate a realistic session: ~30 seconds at 16kHz with 1024 chunk size
        // That's about 469 chunks
        // window=5s (~78 chunks), lag=1s (~16 chunks), periodic=1s (~16 chunks)
        let window_chunks = 78;
        let lag_chunks = 16;
        let periodic_threshold = 16;
        let total_chunks = 469;

        let mut sim =
            TranscriptionWindowSimulator::new(window_chunks, lag_chunks, periodic_threshold);
        let transcribed = sim.simulate(total_chunks);

        // Verify all chunks are covered
        let expected: HashSet<usize> = (0..total_chunks).collect();
        let missing: HashSet<_> = expected.difference(&transcribed).collect();
        assert!(
            missing.is_empty(),
            "Missing chunks: {:?} (count: {})",
            missing,
            missing.len()
        );

        // Verify no overlaps
        let overlaps = sim.get_overlapping_chunks();
        assert!(
            overlaps.is_empty(),
            "Overlapping chunks: {:?} (count: {})",
            overlaps,
            overlaps.len()
        );
    }

    #[test]
    fn test_no_gaps_various_sizes() {
        // Test many different total sizes to catch edge cases
        let window_chunks = 10;
        let lag_chunks = 3;
        let periodic_threshold = 4;

        for total in 1..=100 {
            let mut sim =
                TranscriptionWindowSimulator::new(window_chunks, lag_chunks, periodic_threshold);
            let transcribed = sim.simulate(total);

            let expected: HashSet<usize> = (0..total).collect();
            assert_eq!(
                transcribed, expected,
                "All {} chunks should be transcribed",
                total
            );

            let overlaps = sim.get_overlapping_chunks();
            assert!(
                overlaps.is_empty(),
                "No overlaps for {} chunks, got {:?}",
                total,
                overlaps
            );
        }
    }

    #[test]
    fn test_lag_larger_than_window() {
        // Edge case: lag > window (unusual but should still work)
        // window=5, lag=10, periodic=3, total=50
        let mut sim = TranscriptionWindowSimulator::new(5, 10, 3);
        let transcribed = sim.simulate(50);

        let expected: HashSet<usize> = (0..50).collect();
        assert_eq!(transcribed, expected, "All chunks should be transcribed");

        let overlaps = sim.get_overlapping_chunks();
        assert!(overlaps.is_empty(), "No overlaps expected");
    }

    #[test]
    fn test_single_chunk() {
        // Edge case: Only one chunk
        let mut sim = TranscriptionWindowSimulator::new(10, 2, 5);
        let transcribed = sim.simulate(1);

        let expected: HashSet<usize> = [0].into_iter().collect();
        assert_eq!(transcribed, expected, "Single chunk should be transcribed");
    }

    #[test]
    fn test_zero_lag() {
        // Edge case: No lag (immediate confirmation)
        // window=5, lag=0, periodic=3, total=20
        let mut sim = TranscriptionWindowSimulator::new(5, 0, 3);
        let transcribed = sim.simulate(20);

        let expected: HashSet<usize> = (0..20).collect();
        assert_eq!(transcribed, expected, "All chunks should be transcribed");

        let overlaps = sim.get_overlapping_chunks();
        assert!(overlaps.is_empty(), "No overlaps expected");
    }

    #[test]
    fn test_periodic_threshold_one() {
        // Edge case: Check every chunk (periodic threshold = 1)
        // This is aggressive but should still produce correct results
        let mut sim = TranscriptionWindowSimulator::new(5, 2, 1);
        let transcribed = sim.simulate(30);

        let expected: HashSet<usize> = (0..30).collect();
        assert_eq!(transcribed, expected, "All chunks should be transcribed");

        let overlaps = sim.get_overlapping_chunks();
        assert!(overlaps.is_empty(), "No overlaps expected");
    }

    #[test]
    fn test_window_boundaries_are_contiguous() {
        // Verify that confirmed windows are contiguous (no gaps between windows)
        let mut sim = TranscriptionWindowSimulator::new(10, 2, 5);
        sim.simulate(100);

        // Check that each window starts where the previous one ended
        for i in 1..sim.confirmed_ranges.len() {
            let prev_end = sim.confirmed_ranges[i - 1].1;
            let curr_start = sim.confirmed_ranges[i].0;
            assert_eq!(
                prev_end,
                curr_start,
                "Window {} should start at {} (where window {} ended), but starts at {}",
                i,
                prev_end,
                i - 1,
                curr_start
            );
        }

        // Check that remaining starts where last confirmed ended
        if let Some((remaining_start, _)) = sim.remaining_range {
            if let Some((_, last_confirmed_end)) = sim.confirmed_ranges.last() {
                assert_eq!(
                    *last_confirmed_end, remaining_start,
                    "Remaining should start at {} (where last confirmed ended), but starts at {}",
                    last_confirmed_end, remaining_start
                );
            }
        }
    }
}
