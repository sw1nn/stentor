use anyhow::{Context, Result};
use async_channel::Sender;
use clap::Parser;
use gtk4::prelude::*;
use gtk4::{glib, Application};
use libadwaita as adw;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc as tokio_mpsc;

mod audio;
mod config;
mod daemon;
mod dialog;
mod keyboard;
mod transcription;

use audio::{AudioChunk, AudioRecorder, RecordingCommand, VadState, VoiceActivityDetector};
use config::Config;
use daemon::{DaemonCommand, DaemonServer};
use dialog::{TranscriptionDialog, TranscriptionState};
use transcription::Transcriber;

#[derive(Parser)]
#[command(name = "sw1nn-transcription-daemon")]
#[command(about = "Real-time transcription daemon", long_about = None)]
#[command(after_help = "Configuration can be set in $XDG_CONFIG_HOME/sw1nn-transcription/config.toml.\nCommand-line arguments override config file settings.")]
struct Cli {
    /// Whisper model size
    #[arg(long, value_parser = ["tiny", "base", "small", "medium", "large"])]
    model: Option<String>,

    /// Language code for transcription (default: from config or 'en')
    #[arg(long)]
    language: Option<String>,

    /// Unix socket path (default: $XDG_RUNTIME_DIR/sw1nn-transcription.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Command to execute with transcribed text. Use {transcription} as placeholder.
    #[arg(long)]
    output_command: Option<String>,

    /// Seconds of silence before stopping recording (default: from config or 1.5)
    #[arg(long)]
    silence_duration: Option<f32>,
}

struct RecordingSession {
    #[allow(dead_code)]
    stream: cpal::Stream,
    #[allow(dead_code)]
    stop_tx: std::sync::mpsc::Sender<RecordingCommand>,
}

// Messages for updating UI from background threads
#[derive(Debug, Clone)]
enum UIMessage {
    UpdateState(TranscriptionState, String, f64),
    #[allow(dead_code)]
    SetMicrophone(String),
    SetText(String),
    SetTextPreview(String),
    Close,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    env_logger::init();

    // Parse CLI arguments
    let cli = Cli::parse();

    // Initialize GTK and libadwaita
    adw::init().context("Failed to initialize libadwaita")?;

    // Load configuration
    let mut config = Config::load().context("Failed to load configuration")?;

    // Override config with CLI arguments
    if let Some(model) = cli.model {
        config.model = model;
    }
    if let Some(language) = cli.language {
        config.language = language;
    }
    if let Some(output_command) = cli.output_command {
        config.output_command = Some(output_command);
    }
    if let Some(silence_duration) = cli.silence_duration {
        config.silence_duration = silence_duration;
    }
    log::info!("Loaded configuration: {:?}", config);
    log::info!("Using silence_duration: {} seconds", config.silence_duration);

    // Load Whisper model (this will download if not present)
    log::info!("Loading Whisper model: {}", config.model);
    let model = config.model.clone();
    let language = config.language.clone();
    let transcriber = tokio::task::spawn_blocking(move || Transcriber::new(model, language))
        .await
        .context("Failed to spawn model loading task")?
        .context("Failed to load Whisper model")?;
    let transcriber = Arc::new(transcriber);
    log::info!("Whisper model loaded successfully");

    // Create daemon server
    let socket_path = if let Some(custom_socket) = cli.socket {
        custom_socket
    } else {
        config.socket_path()?
    };
    let mut server = DaemonServer::new(socket_path)?;
    server.bind().await?;

    // Channel for daemon commands
    let (command_tx, mut command_rx) = tokio_mpsc::channel::<DaemonCommand>(32);

    // Spawn daemon server task
    tokio::spawn(async move {
        if let Err(e) = server.run(command_tx).await {
            log::error!("Daemon server error: {}", e);
        }
    });

    // Create GTK application
    let app = Application::builder()
        .application_id("com.sw1nn.transcription")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    // Add a dummy activate handler to suppress GTK warning
    app.connect_activate(|_| {
        // This daemon is socket-driven, not activation-driven
        // This handler exists only to suppress the GTK warning
    });

    // Hold the application to keep it running even when no windows are shown
    let _hold_guard = app.hold();

    // Application state
    #[allow(clippy::arc_with_non_send_sync)]
    let current_dialog: Arc<Mutex<Option<TranscriptionDialog>>> = Arc::new(Mutex::new(None));
    #[allow(clippy::arc_with_non_send_sync)]
    let current_session: Arc<Mutex<Option<RecordingSession>>> = Arc::new(Mutex::new(None));
    let current_ui_tx: Arc<Mutex<Option<Sender<UIMessage>>>> = Arc::new(Mutex::new(None));
    let current_stop_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<RecordingCommand>>>> = Arc::new(Mutex::new(None));

    // Clone for GTK main loop
    let current_dialog_clone = Arc::clone(&current_dialog);
    let _current_session_clone = Arc::clone(&current_session);
    let current_ui_tx_clone = Arc::clone(&current_ui_tx);
    let current_stop_tx_clone = Arc::clone(&current_stop_tx);
    let app_clone = app.clone();
    let config_clone = config.clone();
    let transcriber_clone = Arc::clone(&transcriber);

    // Setup GTK event loop integration with tokio
    glib::MainContext::default().spawn_local(async move {
        while let Some(command) = command_rx.recv().await {
            log::info!("Processing command: {:?}", command);

            match command {
                DaemonCommand::Start => {
                    let mut dialog_lock = current_dialog_clone.lock().unwrap();

                    if dialog_lock.is_none() {
                        // Create new dialog
                        let mut dialog = TranscriptionDialog::new(&app_clone);

                        // Get microphone info
                        let mic_name = match AudioRecorder::new(16000, config_clone.silence_threshold) {
                            Ok(recorder) => recorder.get_device_name().unwrap_or_else(|_| "Default".to_string()),
                            Err(_) => "Default".to_string(),
                        };
                        dialog.set_microphone_info(&mic_name);

                        // Create UI update channel
                        let (ui_tx, ui_rx) = async_channel::unbounded::<UIMessage>();
                        *current_ui_tx_clone.lock().unwrap() = Some(ui_tx.clone());

                        // Setup UI message receiver
                        let dialog_for_updates = dialog.clone();
                        let dialog_state_clone = Arc::clone(&current_dialog_clone);
                        let ui_tx_state_clone = Arc::clone(&current_ui_tx_clone);
                        glib::MainContext::default().spawn_local(async move {
                            while let Ok(msg) = ui_rx.recv().await {
                                log::debug!("UI message received: {:?}", msg);
                                match msg {
                                    UIMessage::UpdateState(state, text, level) => {
                                        dialog_for_updates.update_state(state, &text, level);
                                    }
                                    UIMessage::SetText(text) => {
                                        dialog_for_updates.set_transcribed_text(&text);
                                    }
                                    UIMessage::SetTextPreview(text) => {
                                        dialog_for_updates.set_text_preview(&text);
                                    }
                                    UIMessage::SetMicrophone(name) => {
                                        dialog_for_updates.set_microphone_info(&name);
                                    }
                                    UIMessage::Close => {
                                        log::info!("Closing dialog and cleaning up state");
                                        dialog_for_updates.close();
                                        // Clean up state
                                        *dialog_state_clone.lock().unwrap() = None;
                                        *ui_tx_state_clone.lock().unwrap() = None;
                                        // Stop processing messages
                                        break;
                                    }
                                }
                            }
                        });

                        // Setup callbacks
                        let stop_tx_clone = Arc::clone(&current_stop_tx_clone);
                        dialog.set_on_manual_stop(move || {
                            log::info!("Manual stop requested");
                            let stop_tx_lock = stop_tx_clone.lock().unwrap();
                            if let Some(ref tx) = *stop_tx_lock {
                                let _ = tx.send(RecordingCommand::Stop);
                            }
                        });

                        let config_clone2 = config_clone.clone();
                        let ui_tx_for_close = ui_tx.clone();
                        dialog.set_on_send_text(move |text| {
                            log::info!("Sending text: {}", text);
                            if let Some(ref cmd) = config_clone2.output_command {
                                execute_output_command(cmd, &text);
                            }
                            // Close dialog
                            let _ = ui_tx_for_close.send_blocking(UIMessage::Close);
                        });

                        let ui_tx_for_cancel = ui_tx.clone();
                        dialog.set_on_cancel(move || {
                            log::info!("Cancelled");
                            let _ = ui_tx_for_cancel.send_blocking(UIMessage::Close);
                        });

                        dialog.setup_key_handlers();

                        // Start recording
                        dialog.update_state(TranscriptionState::Recording, "Listening...", 0.0);
                        dialog.present();

                        // Start recording in background
                        let config_clone = config_clone.clone();
                        let transcriber_clone = Arc::clone(&transcriber_clone);
                        let ui_tx_for_recording = ui_tx.clone();
                        let stop_tx_storage = Arc::clone(&current_stop_tx_clone);

                        thread::spawn(move || {
                            match start_recording_session(
                                config_clone,
                                transcriber_clone,
                                ui_tx_for_recording,
                                stop_tx_storage.clone(),
                            ) {
                                Ok(_) => {
                                    log::info!("Recording session completed");
                                }
                                Err(e) => {
                                    log::error!("Recording session error: {}", e);
                                }
                            }
                            // Clear stop_tx when done
                            *stop_tx_storage.lock().unwrap() = None;
                        });

                        *dialog_lock = Some(dialog);
                    } else {
                        // Dialog already exists, bring to front
                        if let Some(ref dialog) = *dialog_lock {
                            dialog.present();
                        }
                    }
                }
                DaemonCommand::Stop => {
                    // Trigger manual stop
                    let stop_tx_lock = current_stop_tx_clone.lock().unwrap();
                    if let Some(ref tx) = *stop_tx_lock {
                        let _ = tx.send(RecordingCommand::Stop);
                    }
                }
                DaemonCommand::Status => {
                    log::info!("Status: running");
                }
                DaemonCommand::Quit => {
                    log::info!("Quitting daemon");
                    app_clone.quit();
                    break;
                }
            }
        }
    });

    // Run GTK application (pass empty args to avoid GTK trying to parse our CLI args)
    app.run_with_args::<String>(&[]);

    Ok(())
}

fn start_recording_session(
    config: Config,
    transcriber: Arc<Transcriber>,
    ui_tx: Sender<UIMessage>,
    stop_tx_storage: Arc<Mutex<Option<std::sync::mpsc::Sender<RecordingCommand>>>>,
) -> Result<()> {
    log::info!("Starting recording session");

    // Create audio recorder
    let recorder = AudioRecorder::new(16000, config.silence_threshold)?;

    // Channels for audio data
    let (chunk_tx, chunk_rx) = std::sync::mpsc::channel::<AudioChunk>();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<RecordingCommand>();
    let stop_rx = Arc::new(Mutex::new(stop_rx));

    // Store stop_tx BEFORE starting recording so Escape key can use it
    *stop_tx_storage.lock().unwrap() = Some(stop_tx);

    // Start audio stream
    let stream = recorder.start_recording(chunk_tx, stop_rx)?;

    // Process audio in this thread
    log::info!(
        "VAD initialized: silence_threshold={}, silence_duration={}, min_speech_duration={}",
        config.silence_threshold,
        config.silence_duration,
        config.min_speech_duration
    );
    let vad = VoiceActivityDetector::new(
        config.silence_threshold,
        config.silence_duration,
        config.min_speech_duration,
        16000,
    );

    let mut recorded_audio: Vec<Vec<f32>> = Vec::new();
    let mut vad_state = VadState::Idle;
    let mut silence_chunks = 0;
    let mut speech_chunks = 0;

    // Update dialog
    let _ = ui_tx.send_blocking(UIMessage::UpdateState(
        TranscriptionState::Recording,
        "Listening...".to_string(),
        0.0,
    ));

    loop {
        match chunk_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(chunk) => {
                let rms = chunk.rms;
                log::debug!("Received audio chunk: RMS = {}", rms);

                // Update UI with audio level
                let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                    TranscriptionState::Recording,
                    "Listening...".to_string(),
                    rms as f64,
                ));

                // Process through VAD
                let result = vad.process_chunk(rms, vad_state, silence_chunks, speech_chunks);
                vad_state = result.state;

                match vad_state {
                    VadState::Speaking => {
                        silence_chunks = 0;
                        speech_chunks += 1;
                        recorded_audio.push(chunk.data);
                    }
                    VadState::SilenceAfterSpeech => {
                        silence_chunks += 1;
                        recorded_audio.push(chunk.data);

                        if result.should_stop {
                            log::info!("VAD detected end of speech");
                            break;
                        }
                    }
                    VadState::Idle => {
                        // Still waiting for speech
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                log::info!("Audio stream disconnected");
                break;
            }
        }
    }

    // Keep stream alive
    drop(stream);

    // Check if we have enough audio
    if recorded_audio.is_empty() || speech_chunks == 0 {
        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
            TranscriptionState::Error,
            "No speech detected".to_string(),
            0.0,
        ));
        thread::sleep(std::time::Duration::from_secs(2));
        let _ = ui_tx.send_blocking(UIMessage::Close);
        return Ok(());
    }

    // Transcribe
    log::info!("Transcribing {} chunks of audio", recorded_audio.len());
    let _ = ui_tx.send_blocking(UIMessage::UpdateState(
        TranscriptionState::Processing,
        "Transcribing...".to_string(),
        0.0,
    ));

    // Flatten audio data
    let audio_flat: Vec<f32> = recorded_audio.into_iter().flatten().collect();
    log::info!("Total audio samples: {}", audio_flat.len());

    // Transcribe in blocking operation with progressive updates
    let ui_tx_for_segments = ui_tx.clone();
    let accumulated_text = Arc::new(Mutex::new(String::new()));
    let accumulated_text_clone = Arc::clone(&accumulated_text);

    let result = transcriber.transcribe_with_callback(&audio_flat, move |segment| {
        // Skip [BLANK_AUDIO] segments
        if segment.trim() == "[BLANK_AUDIO]" {
            return;
        }
        let mut text = accumulated_text_clone.lock().unwrap();
        text.push_str(segment);
        let _ = ui_tx_for_segments.send_blocking(UIMessage::SetTextPreview(text.clone()));
    })?;
    log::info!("Transcription result: {}", result);

    // Filter out [BLANK_AUDIO] from the result
    let cleaned_result = result.replace("[BLANK_AUDIO]", "").trim().to_string();

    if cleaned_result.is_empty() {
        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
            TranscriptionState::Error,
            "No speech detected".to_string(),
            0.0,
        ));
        thread::sleep(std::time::Duration::from_secs(2));
        let _ = ui_tx.send_blocking(UIMessage::Close);
        return Ok(());
    }

    // Show result in dialog
    let _ = ui_tx.send_blocking(UIMessage::SetText(cleaned_result));
    let _ = ui_tx.send_blocking(UIMessage::UpdateState(
        TranscriptionState::Reviewing,
        "Ready to send".to_string(),
        0.0,
    ));

    Ok(())
}

fn execute_output_command(command_template: &str, text: &str) {
    let command = command_template.replace("{transcription}", text);
    log::info!("Executing command: {}", command);

    match std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .output()
    {
        Ok(output) => {
            if !output.status.success() {
                log::error!("Command failed: {}", String::from_utf8_lossy(&output.stderr));
            }
        }
        Err(e) => {
            log::error!("Failed to execute command: {}", e);
        }
    }
}
