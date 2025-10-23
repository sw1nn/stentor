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
    #[allow(dead_code)]
    SetMicrophone(String),
    SetText(String),
    SetTextPreview(String),
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
    let recorder = AudioRecorder::new(16000, source)?;

    // Get the ACTUAL sample rate the device is using (not what we requested)
    let actual_sample_rate = recorder.get_actual_sample_rate()?;

    // Channels for audio data
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<AudioChunk>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<RecordingCommand>();
    let stop_rx = Arc::new(Mutex::new(stop_rx));

    // Store stop_tx BEFORE starting recording so Escape key can use it
    *stop_tx_storage.lock().unwrap() = Some(stop_tx);

    // Start audio stream (runs in background thread)
    recorder.start_recording(chunk_tx, stop_rx)?;

    // Use VAD to detect speech start and silence
    // IMPORTANT: Use the ACTUAL sample rate from the device, not the requested rate
    // silence_duration is not used for stopping, only for detecting silence state
    // stop_silence_duration (config.silence_duration) is used to actually stop the session
    tracing::info!(
        "VAD initialized: silence_threshold={}, min_speech_duration={}, stop_silence_duration={}, sample_rate={}",
        config.silence_threshold,
        config.min_speech_duration,
        config.silence_duration,
        actual_sample_rate
    );
    let vad = VoiceActivityDetector::new(
        config.silence_threshold,
        config.min_speech_duration,
        config.silence_duration, // Silence duration to trigger stop
        actual_sample_rate,      // Use ACTUAL sample rate from device
    );

    let mut recorded_audio: Vec<Vec<f32>> = Vec::new();
    let accumulated_text = Arc::new(Mutex::new(String::new()));
    let mut vad_state = VadState::Idle;
    let mut previous_vad_state;
    let mut silence_chunks = 0;
    let mut speech_chunks = 0;
    let mut transcription_started = false;

    // Track if a transcription is currently in flight to prevent concurrent transcriptions
    // Using Mutex for RAII - holding the lock means transcription is in flight
    let transcription_in_flight = Arc::new(Mutex::new(()));

    let transcriber_clone = Arc::clone(&transcriber);
    let ui_tx_for_transcription = ui_tx.clone();
    let accumulated_text_clone = Arc::clone(&accumulated_text);

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
                        recorded_audio.push(chunk.data);

                        // Switch to transcription mode once speech starts
                        if !transcription_started {
                            transcription_started = true;
                            tracing::info!("Speech detected! Transcription will run on pause...");
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Processing,
                                "Recording... (pause or press Escape to transcribe)".to_string(),
                                rms as f64,
                            ));
                        } else {
                            // Update UI with audio level
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Processing,
                                "Recording... (pause or press Escape to transcribe)".to_string(),
                                rms as f64,
                            ));
                        }
                    }
                    SilenceAfterSpeech => {
                        silence_chunks += 1;
                        recorded_audio.push(chunk.data);

                        // Update UI with audio level
                        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                            TranscriptionState::Processing,
                            "Transcribing... (press Escape to finish)".to_string(),
                            rms as f64,
                        ));

                        // Trigger transcription on transition to SilenceAfterSpeech
                        // Only if no transcription is currently in flight
                        if transitioned_to_silence && !recorded_audio.is_empty() {
                            // Try to acquire the lock (non-blocking)
                            // If we can't acquire it, transcription is already in flight
                            if let Ok(_guard) = transcription_in_flight.try_lock() {
                                tracing::info!(
                                    "Pause detected! Transcribing {} chunks",
                                    recorded_audio.len()
                                );

                                // Clone all accumulated audio for transcription
                                let audio_flat: Vec<f32> =
                                    recorded_audio.iter().flatten().copied().collect();
                                let transcriber_ref = Arc::clone(&transcriber_clone);
                                let ui_tx_transcribe = ui_tx_for_transcription.clone();
                                let text_accumulator = Arc::clone(&accumulated_text_clone);
                                let in_flight_mutex = Arc::clone(&transcription_in_flight);

                                // Spawn transcription in background to avoid blocking recording
                                thread::spawn(move || {
                                    // Acquire the lock for the duration of transcription (RAII)
                                    let _guard = in_flight_mutex.lock().unwrap();

                                    // Build up full text from all segments
                                    let full_text = Arc::new(Mutex::new(String::new()));
                                    let full_text_clone = Arc::clone(&full_text);

                                    match transcriber_ref.transcribe_with_realtime_callback(
                                        &audio_flat,
                                        move |segment| {
                                            // Skip [BLANK_AUDIO] segments
                                            if segment.trim() == "[BLANK_AUDIO]" {
                                                return;
                                            }
                                            let mut text = full_text_clone.lock().unwrap();
                                            if !text.is_empty() && !text.ends_with(' ') {
                                                text.push(' ');
                                            }
                                            text.push_str(segment.trim());
                                        },
                                    ) {
                                        Ok(_) => {
                                            // Take ownership of text to avoid cloning twice
                                            let complete_text = std::mem::take(&mut *full_text.lock().unwrap());
                                            if !complete_text.is_empty() {
                                                tracing::info!(
                                                    "Transcription complete: '{}'",
                                                    complete_text
                                                );
                                                // Clone once for text_accumulator, move to UI message
                                                *text_accumulator.lock().unwrap() = complete_text.clone();
                                                let _ = ui_tx_transcribe.send_blocking(
                                                    UIMessage::SetTextPreview(complete_text),
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("Transcription failed: {}", e);
                                        }
                                    }
                                    // _guard is dropped here, releasing the lock automatically
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

    // Get final transcription result
    let final_text = accumulated_text.lock().unwrap().clone();
    tracing::info!("Final text from accumulated_text: '{}'", final_text);

    // Check if we should auto-send to specific slots (atomically retrieve and clear)
    let auto_slots = {
        let mut slots = auto_send_slots.lock().unwrap();
        let result = slots.clone();
        slots.clear(); // Clear for next session
        result
    };
    tracing::info!("Auto-send slots: {:?}", auto_slots);

    if final_text.is_empty() {
        tracing::info!("Final text is empty, checking recorded audio");
        // No text yet, do one final transcription
        if !recorded_audio.is_empty() {
            tracing::info!(
                "Performing final transcription of {} chunks",
                recorded_audio.len()
            );
            let audio_flat: Vec<f32> = recorded_audio.into_iter().flatten().collect();

            match transcriber.transcribe(&audio_flat) {
                Ok(result) => {
                    let cleaned = result.replace("[BLANK_AUDIO]", "").trim().to_string();
                    tracing::info!("Final transcription result (cleaned): '{}'", cleaned);
                    if !cleaned.is_empty() {
                        // Auto-send or show for review
                        if !auto_slots.is_empty() {
                            // Send to all requested slots
                            for slot in &auto_slots {
                                tracing::info!(
                                    "Auto-sending to slot {} with text: '{}'",
                                    slot,
                                    cleaned
                                );
                                let _ = ui_tx.send_blocking(UIMessage::AutoSendText(cleaned.clone(), *slot));
                            }
                            tracing::info!("AutoSendText messages sent to {} slots", auto_slots.len());
                            // Send Close message after all AutoSendText messages to cleanup and hide dialog
                            let _ = ui_tx.send_blocking(UIMessage::Close);
                        } else {
                            tracing::info!("Showing text for review");
                            let _ = ui_tx.send_blocking(UIMessage::SetText(cleaned.clone()));
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Reviewing,
                                "Ready to send".to_string(),
                                0.0,
                            ));
                        }
                        return Ok(());
                    } else {
                        tracing::info!("Cleaned text is empty");
                    }
                }
                Err(e) => {
                    tracing::error!("Final transcription failed: {}", e);
                }
            }
        }

        // Still no text - show error
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
                "Auto-sending final_text to slot {} with text: '{}'",
                slot,
                final_text
            );
            let _ = ui_tx.send_blocking(UIMessage::AutoSendText(final_text.clone(), *slot));
        }
        tracing::info!("AutoSendText messages sent to {} slots (from accumulated text)", auto_slots.len());
        // Send Close message after all AutoSendText messages to cleanup and hide dialog
        let _ = ui_tx.send_blocking(UIMessage::Close);
    } else {
        tracing::info!("Showing final_text for review");
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
