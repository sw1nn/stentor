use anyhow::Result;
use async_channel::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::audio::{AudioChunk, AudioRecorder, RecordingCommand, VadState, VoiceActivityDetector};
use crate::config::Config;
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
    let device_name = recorder.get_device_name().unwrap_or_else(|_| "Unknown".to_string());
    tracing::info!(microphone = %device_name, "Recording with microphone: {device_name}");

    // Get the ACTUAL sample rate the device is using (not what we requested)
    let actual_sample_rate = recorder.get_actual_sample_rate()?;

    // Channels for audio data
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<AudioChunk>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<RecordingCommand>();
    let stop_rx = Arc::new(Mutex::new(stop_rx));

    // Store stop_tx BEFORE starting recording so Escape key can use it
    *stop_tx_storage.lock().expect("Mutex poisoned") = Some(stop_tx);

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
        match chunk_rx.recv_timeout(std::time::Duration::from_millis(100)) {
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
                            tracing::info!("Speech detected! Transcription will run periodically...");
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
                        {
                            if let Ok(_guard) = transcription_in_flight.try_lock() {
                                chunks_since_last_transcription = 0;

                                let total_chunks = recorded_audio.len();
                                let current_confirmed =
                                    *confirmed_chunks.lock().expect("Mutex poisoned");

                                // Calculate settled audio (older than lag)
                                let settled_chunks = total_chunks.saturating_sub(lag_chunks);

                                // Check if we have a full window of settled audio to confirm
                                let can_confirm =
                                    settled_chunks >= current_confirmed + window_chunks;

                                // Clone audio for transcription
                                let audio_for_transcription: Vec<Vec<f32>> =
                                    recorded_audio.clone();

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
                                    let _guard = in_flight_mutex.lock().expect("Mutex poisoned");

                                    let mut current_confirmed_text =
                                        confirmed_text_ref.lock().expect("Mutex poisoned").clone();
                                    let mut current_confirmed_idx =
                                        *confirmed_chunks_ref.lock().expect("Mutex poisoned");

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
                                                let cleaned =
                                                    text.replace("[BLANK_AUDIO]", "").trim().to_string();
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
                                        *confirmed_text_ref.lock().expect("Mutex poisoned") =
                                            current_confirmed_text.clone();
                                        *confirmed_chunks_ref.lock().expect("Mutex poisoned") =
                                            current_confirmed_idx;
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
                            }
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
                            if let Ok(_guard) = transcription_in_flight.try_lock() {
                                chunks_since_last_transcription = 0;

                                let total_chunks = recorded_audio.len();
                                let current_confirmed =
                                    *confirmed_chunks.lock().expect("Mutex poisoned");

                                // Calculate settled audio (older than lag)
                                let settled_chunks = total_chunks.saturating_sub(lag_chunks);

                                // Check if we have a full window of settled audio to confirm
                                let can_confirm =
                                    settled_chunks >= current_confirmed + window_chunks;

                                // Clone audio for transcription
                                let audio_for_transcription: Vec<Vec<f32>> =
                                    recorded_audio.clone();

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
                                    let _guard = in_flight_mutex.lock().expect("Mutex poisoned");

                                    let mut current_confirmed_text =
                                        confirmed_text_ref.lock().expect("Mutex poisoned").clone();
                                    let mut current_confirmed_idx =
                                        *confirmed_chunks_ref.lock().expect("Mutex poisoned");

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
                                                let cleaned =
                                                    text.replace("[BLANK_AUDIO]", "").trim().to_string();
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
                                        *confirmed_text_ref.lock().expect("Mutex poisoned") =
                                            current_confirmed_text.clone();
                                        *confirmed_chunks_ref.lock().expect("Mutex poisoned") =
                                            current_confirmed_idx;
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

    // Get confirmed text so far
    let mut final_text = confirmed_text.lock().expect("Mutex poisoned").clone();
    let current_confirmed_idx = *confirmed_chunks.lock().expect("Mutex poisoned");

    tracing::info!(
        confirmed_text = %final_text,
        confirmed_chunks = current_confirmed_idx,
        total_chunks = recorded_audio.len(),
        "Final transcription - processing remaining audio"
    );

    // Check if we should auto-send to specific slots (atomically retrieve and clear)
    let auto_slots = {
        let mut slots = auto_send_slots.lock().expect("Mutex poisoned");
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
        thread::sleep(std::time::Duration::from_secs(2));
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
        tracing::info!(
            slots = auto_slots.len(),
            "AutoSendText messages sent"
        );
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

pub fn execute_output_command(command_template: &str, text: &str, slot_num: usize) {
    tracing::info!(
        transcription = text,
        slot_num,
        command_template,
        "Executing command"
    );

    // Pass transcription and slot number via environment variables to prevent shell injection
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command_template)
        .env("TRANSCRIPTION", text)
        .env("SLOT", slot_num.to_string());

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
