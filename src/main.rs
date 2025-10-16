use anyhow::{Context, Result};
use async_channel::Sender;
use clap::Parser;
use gtk4::prelude::*;
use gtk4::{glib, Application};
use libadwaita as adw;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc as tokio_mpsc;

mod audio;
mod config;
mod daemon;
mod dialog;
mod keyboard;
mod kitty;
mod palette;
mod source_mute;
mod transcription;

use audio::{AudioChunk, AudioRecorder, RecordingCommand, VadState, VoiceActivityDetector};
use config::Config;
use daemon::{DaemonCommand, DaemonServer};
use dialog::{DestinationSlot, TranscriptionDialog, TranscriptionState};
use kitty::{find_claude_windows, list_kitty_windows, set_background_color};
use palette::Palette;
use source_mute::SourceMuteManager;
use transcription::Transcriber;

#[derive(Parser)]
#[command(name = "stentord")]
#[command(about = "Real-time transcription daemon", long_about = None)]
#[command(after_help = "Configuration can be set in $XDG_CONFIG_HOME/stentor/config.toml.\nCommand-line arguments override config file settings.")]
struct Cli {
    /// Whisper model size
    #[arg(long, value_parser = ["tiny", "base", "small", "medium", "large"])]
    model: Option<String>,

    /// Language code for transcription (default: from config or 'en')
    #[arg(long)]
    language: Option<String>,

    /// Unix socket path (default: $XDG_RUNTIME_DIR/stentor.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Command to execute with transcribed text. Use $TRANSCRIPTION environment variable.
    #[arg(long)]
    output_command: Option<String>,

    /// Seconds of silence before stopping recording (default: from config or 1.5)
    #[arg(long)]
    silence_duration: Option<f32>,

    /// Enable Kitty terminal integration for multi-destination support
    #[arg(long)]
    kitty: bool,

    /// Disable logging output (overrides RUST_LOG)
    #[arg(short, long)]
    quiet: bool,
}

// RecordingSession is no longer needed - audio streams are managed
// entirely within the AudioRecorder's background thread

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
    // Parse CLI arguments first to check for --quiet flag
    let cli = Cli::parse();

    // Initialize logging with appropriate level
    // If --quiet is set, disable logging completely
    // Otherwise, default to info level if RUST_LOG is not set
    if !cli.quiet {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
            )
            .init();
    }

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
    if cli.kitty {
        config.kitty_mode = true;
    }
    tracing::info!("Loaded configuration: {:?}", config);
    tracing::info!("Using silence_duration: {} seconds", config.silence_duration);

    // Log build-time compilation flags for debugging
    tracing::info!("Build-time flags:");
    tracing::info!("  Profile: {}", env!("BUILD_PROFILE"));
    tracing::info!("  Opt Level: {}", env!("BUILD_OPT_LEVEL"));
    tracing::info!("  Debug: {}", env!("BUILD_DEBUG"));
    tracing::info!("  CFLAGS: {}", env!("BUILD_CFLAGS"));
    tracing::info!("  CXXFLAGS: {}", env!("BUILD_CXXFLAGS"));
    tracing::info!("  LDFLAGS: {}", env!("BUILD_LDFLAGS"));
    tracing::info!("  RUSTFLAGS: {}", env!("BUILD_RUSTFLAGS"));
    tracing::info!("  CARGO_PROFILE_RELEASE_LTO: {}", env!("BUILD_CARGO_PROFILE_RELEASE_LTO"));
    tracing::info!("  CARGO_PROFILE_RELEASE_CODEGEN_UNITS: {}", env!("BUILD_CARGO_PROFILE_RELEASE_CODEGEN_UNITS"));

    // Load Whisper model (this will download if not present)
    tracing::info!("Loading Whisper model: {}", config.model);
    let model = config.model.clone();
    let language = config.language.clone();
    let transcriber = tokio::task::spawn_blocking(move || Transcriber::new(model, language))
        .await
        .context("Failed to spawn model loading task")?
        .context("Failed to load Whisper model")?;
    let transcriber = Arc::new(transcriber);
    tracing::info!("Whisper model loaded successfully");

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
            tracing::error!("Daemon server error: {}", e);
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
    // Use Rc<RefCell<>> for GTK widgets since all GTK operations happen on the main thread
    let current_dialog: Rc<RefCell<Option<TranscriptionDialog>>> = Rc::new(RefCell::new(None));
    let current_ui_tx: Rc<RefCell<Option<Sender<UIMessage>>>> = Rc::new(RefCell::new(None));
    // Keep Arc<Mutex<>> for stop_tx since it's shared with background threads
    let current_stop_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<RecordingCommand>>>> = Arc::new(Mutex::new(None));

    // Clone for GTK main loop
    let current_dialog_clone = Rc::clone(&current_dialog);
    let current_ui_tx_clone = Rc::clone(&current_ui_tx);
    let current_stop_tx_clone = Arc::clone(&current_stop_tx);
    let app_clone = app.clone();
    let config_clone = config.clone();
    let transcriber_clone = Arc::clone(&transcriber);

    // Setup GTK event loop integration with tokio
    glib::MainContext::default().spawn_local(async move {
        while let Some(command) = command_rx.recv().await {
            tracing::info!("Processing command: {:?}", command);

            match command {
                DaemonCommand::Start { unmute_source, source } => {
                    let mut dialog_ref = current_dialog_clone.borrow_mut();

                    if dialog_ref.is_none() {
                        // Create new dialog
                        let mut dialog = TranscriptionDialog::new(&app_clone);

                        // Get source info
                        let source_name = match AudioRecorder::new(16000, source.clone()) {
                            Ok(recorder) => recorder.get_device_name().unwrap_or_else(|_| "Default".to_string()),
                            Err(_) => "Default".to_string(),
                        };
                        dialog.set_source_info(&source_name);

                        // Setup Kitty integration if enabled
                        let kitty_windows = if config_clone.kitty_mode {
                            tracing::info!("Kitty mode enabled, discovering Claude windows...");
                            match list_kitty_windows().and_then(|data| Ok(find_claude_windows(&data))) {
                                Ok(windows) => {
                                    tracing::info!("Found {} Claude windows", windows.len());

                                    // Create palette (using default dark theme color)
                                    let palette = Palette::new("#1e1e2e");

                                    // Setup destinations and colors
                                    let mut destinations = Vec::new();
                                    for (i, window) in windows.iter().enumerate().take(4) {
                                        let slot_num = i + 1;
                                        if let Some((_name, color_hex)) = palette.get_slot_color(slot_num) {
                                            // Extract just the directory name from the full CWD path
                                            let label = window.cwd.split('/').last().unwrap_or(&window.cwd).to_string();
                                            destinations.push(DestinationSlot::with_label(slot_num, label, color_hex.to_string()));

                                            // Set the window background color to match
                                            if let Err(e) = set_background_color(color_hex, window.id) {
                                                tracing::warn!("Failed to set background color for window {}: {}", window.id, e);
                                            }
                                        }
                                    }

                                    // Fill remaining slots as empty
                                    for i in windows.len()..4 {
                                        let slot_num = i + 1;
                                        if let Some((_name, color_hex)) = palette.get_slot_color(slot_num) {
                                            destinations.push(DestinationSlot::empty(slot_num, color_hex.to_string()));
                                        }
                                    }

                                    dialog.set_destinations(&destinations);
                                    Some(windows)
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to discover Kitty windows: {}", e);
                                    None
                                }
                            }
                        } else {
                            None
                        };

                        // Create UI update channel with backpressure
                        // Bounded to prevent OOM if UI thread blocks
                        // Capacity of 128 (power of 2) allows ~8 seconds of buffering at typical message rate
                        // and enables compiler optimization of modulo operations to bitwise AND
                        let (ui_tx, ui_rx) = async_channel::bounded::<UIMessage>(128);
                        *current_ui_tx_clone.borrow_mut() = Some(ui_tx.clone());

                        // Setup UI message receiver
                        let dialog_for_updates = dialog.clone();
                        let dialog_state_clone = Rc::clone(&current_dialog_clone);
                        let ui_tx_state_clone = Rc::clone(&current_ui_tx_clone);
                        glib::MainContext::default().spawn_local(async move {
                            while let Ok(msg) = ui_rx.recv().await {
                                tracing::debug!("UI message received: {:?}", msg);
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
                                        dialog_for_updates.set_source_info(&name);
                                    }
                                    UIMessage::Close => {
                                        tracing::info!("Closing dialog and cleaning up state");
                                        dialog_for_updates.close();
                                        // Clean up state
                                        *dialog_state_clone.borrow_mut() = None;
                                        *ui_tx_state_clone.borrow_mut() = None;
                                        // Stop processing messages
                                        break;
                                    }
                                }
                            }
                        });

                        // Setup callbacks
                        let stop_tx_clone = Arc::clone(&current_stop_tx_clone);
                        dialog.set_on_manual_stop(move || {
                            tracing::info!("Manual stop requested");
                            let stop_tx_lock = stop_tx_clone.lock().unwrap();
                            if let Some(ref tx) = *stop_tx_lock {
                                let _ = tx.send(RecordingCommand::Stop);
                            }
                        });

                        let config_clone2 = config_clone.clone();
                        let ui_tx_for_close = ui_tx.clone();
                        let kitty_windows_for_callback = kitty_windows.clone();
                        dialog.set_on_send_text(move |text, dest_num| {
                            tracing::info!("Sending text to destination {}: {}", dest_num, text);

                            // If dest_num is 0 (Ctrl+Enter), use default output command
                            // If dest_num is 1-4 (Ctrl+1-4), use corresponding output command
                            let cmd = if dest_num == 0 {
                                config_clone2.output_command.as_ref()
                            } else {
                                match dest_num {
                                    1 => config_clone2.output_command_1.as_ref(),
                                    2 => config_clone2.output_command_2.as_ref(),
                                    3 => config_clone2.output_command_3.as_ref(),
                                    4 => config_clone2.output_command_4.as_ref(),
                                    _ => None,
                                }
                            };

                            if let Some(cmd_str) = cmd {
                                // If Kitty mode is enabled and we have windows, set KITTY_WINDOW_ID env var
                                if config_clone2.kitty_mode {
                                    if let Some(ref windows) = kitty_windows_for_callback {
                                        if dest_num > 0 && dest_num <= windows.len() {
                                            let window_id = windows[dest_num - 1].id;
                                            tracing::info!("Setting KITTY_WINDOW_ID={} for destination {}", window_id, dest_num);
                                            execute_output_command_with_window(cmd_str, &text, Some(window_id));
                                        } else {
                                            execute_output_command(cmd_str, &text);
                                        }
                                    } else {
                                        execute_output_command(cmd_str, &text);
                                    }
                                } else {
                                    execute_output_command(cmd_str, &text);
                                }
                            }

                            // Close dialog
                            let _ = ui_tx_for_close.send_blocking(UIMessage::Close);
                        });

                        let ui_tx_for_cancel = ui_tx.clone();
                        dialog.set_on_cancel(move || {
                            tracing::info!("Cancelled");
                            let _ = ui_tx_for_cancel.send_blocking(UIMessage::Close);
                        });

                        dialog.setup_key_handlers();

                        let ui_tx_for_close_handler = ui_tx.clone();
                        dialog.connect_close_handler(move || {
                            let _ = ui_tx_for_close_handler.send_blocking(UIMessage::Close);
                        });

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
                                unmute_source,
                                source,
                            ) {
                                Ok(_) => {
                                    tracing::info!("Recording session completed");
                                }
                                Err(e) => {
                                    tracing::error!("Recording session error: {}", e);
                                }
                            }
                            // Clear stop_tx when done
                            *stop_tx_storage.lock().unwrap() = None;
                        });

                        *dialog_ref = Some(dialog);
                    } else {
                        // Dialog already exists, bring to front
                        if let Some(ref dialog) = *dialog_ref {
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
                    tracing::info!("Status: running");
                }
                DaemonCommand::Quit => {
                    tracing::info!("Quitting daemon");
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
    unmute_source: bool,
    source: Option<String>,
) -> Result<()> {
    tracing::info!("Starting recording session");

    // Unmute source if requested
    let _source_manager = if unmute_source {
        match SourceMuteManager::unmute_if_needed() {
            Ok(manager) => Some(manager),
            Err(e) => {
                tracing::warn!("Failed to manage source mute state: {}. Continuing anyway.", e);
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
        config.silence_duration,  // Silence duration to trigger stop
        actual_sample_rate,  // Use ACTUAL sample rate from device
    );

    let mut recorded_audio: Vec<Vec<f32>> = Vec::new();
    let accumulated_text = Arc::new(Mutex::new(String::new()));
    let mut vad_state = VadState::Idle;
    let mut silence_chunks = 0;
    let mut speech_chunks = 0;
    let mut transcription_started = false;

    // For continuous transcription during recording
    // Re-transcribe all audio every 2 seconds for better accuracy (Whisper needs context)
    let transcription_interval = std::time::Duration::from_millis(2000);
    let mut last_transcription = std::time::Instant::now();
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
                tracing::debug!("Received audio chunk: RMS = {}", rms);

                // Process through VAD
                let result = vad.process_chunk(rms, vad_state, silence_chunks, speech_chunks);
                tracing::debug!("VAD: state transition {:?} -> {:?}, silence_chunks={}, should_stop={}",
                    vad_state, result.state, silence_chunks, result.should_stop);
                vad_state = result.state;

                match vad_state {
                    VadState::Speaking => {
                        silence_chunks = 0;
                        speech_chunks += 1;
                        recorded_audio.push(chunk.data);

                        // Switch to transcription mode once speech starts
                        if !transcription_started {
                            transcription_started = true;
                            tracing::info!("Speech detected! Starting continuous transcription...");
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Processing,
                                "Transcribing... (press Escape or stop speaking to finish)".to_string(),
                                rms as f64,
                            ));
                        } else {
                            // Update UI with audio level during transcription
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Processing,
                                "Transcribing... (press Escape or stop speaking to finish)".to_string(),
                                rms as f64,
                            ));
                        }

                        // Continuous transcription - re-transcribe ALL audio for accuracy
                        if last_transcription.elapsed() >= transcription_interval && !recorded_audio.is_empty() {
                            tracing::info!("Re-transcribing all {} chunks for accuracy", recorded_audio.len());
                            last_transcription = std::time::Instant::now();

                            // Clone all accumulated audio for transcription
                            let audio_flat: Vec<f32> = recorded_audio.iter().flatten().copied().collect();
                            let transcriber_ref = Arc::clone(&transcriber_clone);
                            let ui_tx_transcribe = ui_tx_for_transcription.clone();
                            let text_accumulator = Arc::clone(&accumulated_text_clone);

                            // Spawn transcription in background to avoid blocking recording
                            thread::spawn(move || {
                                // Build up full text from all segments
                                let full_text = Arc::new(Mutex::new(String::new()));
                                let full_text_clone = Arc::clone(&full_text);

                                match transcriber_ref.transcribe_with_realtime_callback(&audio_flat, move |segment| {
                                    // Skip [BLANK_AUDIO] segments
                                    if segment.trim() == "[BLANK_AUDIO]" {
                                        return;
                                    }
                                    let mut text = full_text_clone.lock().unwrap();
                                    if !text.is_empty() && !text.ends_with(' ') {
                                        text.push(' ');
                                    }
                                    text.push_str(segment.trim());
                                }) {
                                    Ok(_) => {
                                        let complete_text = full_text.lock().unwrap().clone();
                                        if !complete_text.is_empty() {
                                            // Replace accumulated text with the complete re-transcription
                                            *text_accumulator.lock().unwrap() = complete_text.clone();
                                            tracing::info!("Complete transcription: '{}'", complete_text);
                                            let _ = ui_tx_transcribe.send_blocking(UIMessage::SetTextPreview(complete_text));
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("Continuous transcription failed: {}", e);
                                    }
                                }
                            });
                        }
                    }
                    VadState::SilenceAfterSpeech => {
                        silence_chunks += 1;
                        recorded_audio.push(chunk.data);

                        // Update UI with audio level
                        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                            TranscriptionState::Processing,
                            "Transcribing... (press Escape or stop speaking to finish)".to_string(),
                            rms as f64,
                        ));

                        // Continue transcription during pauses - re-transcribe ALL audio for accuracy
                        if last_transcription.elapsed() >= transcription_interval && !recorded_audio.is_empty() {
                            tracing::info!("Re-transcribing all {} chunks during pause", recorded_audio.len());
                            last_transcription = std::time::Instant::now();

                            // Clone all accumulated audio for transcription
                            let audio_flat: Vec<f32> = recorded_audio.iter().flatten().copied().collect();
                            let transcriber_ref = Arc::clone(&transcriber_clone);
                            let ui_tx_transcribe = ui_tx_for_transcription.clone();
                            let text_accumulator = Arc::clone(&accumulated_text_clone);

                            // Spawn transcription in background to avoid blocking recording
                            thread::spawn(move || {
                                // Build up full text from all segments
                                let full_text = Arc::new(Mutex::new(String::new()));
                                let full_text_clone = Arc::clone(&full_text);

                                match transcriber_ref.transcribe_with_realtime_callback(&audio_flat, move |segment| {
                                    // Skip [BLANK_AUDIO] segments
                                    if segment.trim() == "[BLANK_AUDIO]" {
                                        return;
                                    }
                                    let mut text = full_text_clone.lock().unwrap();
                                    if !text.is_empty() && !text.ends_with(' ') {
                                        text.push(' ');
                                    }
                                    text.push_str(segment.trim());
                                }) {
                                    Ok(_) => {
                                        let complete_text = full_text.lock().unwrap().clone();
                                        if !complete_text.is_empty() {
                                            // Replace accumulated text with the complete re-transcription
                                            *text_accumulator.lock().unwrap() = complete_text.clone();
                                            tracing::info!("Complete transcription during pause: '{}'", complete_text);
                                            let _ = ui_tx_transcribe.send_blocking(UIMessage::SetTextPreview(complete_text));
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("Continuous transcription during pause failed: {}", e);
                                    }
                                }
                            });
                        }

                        if result.should_stop {
                            tracing::info!("VAD detected end of speech (silence duration exceeded)");
                            break;
                        }
                    }
                    VadState::Idle => {
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

    if final_text.is_empty() {
        // No text yet, do one final transcription
        if !recorded_audio.is_empty() {
            tracing::info!("Performing final transcription of {} chunks", recorded_audio.len());
            let audio_flat: Vec<f32> = recorded_audio.into_iter().flatten().collect();

            match transcriber.transcribe(&audio_flat) {
                Ok(result) => {
                    let cleaned = result.replace("[BLANK_AUDIO]", "").trim().to_string();
                    if !cleaned.is_empty() {
                        let _ = ui_tx.send_blocking(UIMessage::SetText(cleaned.clone()));
                        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                            TranscriptionState::Reviewing,
                            "Ready to send".to_string(),
                            0.0,
                        ));
                        return Ok(());
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

    // Show final result in dialog for review
    let _ = ui_tx.send_blocking(UIMessage::SetText(final_text));
    let _ = ui_tx.send_blocking(UIMessage::UpdateState(
        TranscriptionState::Reviewing,
        "Ready to send".to_string(),
        0.0,
    ));

    Ok(())
}

fn execute_output_command(command_template: &str, text: &str) {
    execute_output_command_with_window(command_template, text, None);
}

fn execute_output_command_with_window(command_template: &str, text: &str, window_id: Option<u64>) {
    tracing::info!(transcription = text, command_template, "Executing command");

    // Pass transcription via environment variable to prevent shell injection
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command_template)
        .env("TRANSCRIPTION", text);

    // If window_id is provided, also set KITTY_WINDOW_ID for kitty @ commands
    if let Some(id) = window_id {
        cmd.env("KITTY_WINDOW_ID", id.to_string());
        tracing::info!(kitty_window_id = id, "Set KITTY_WINDOW_ID");
    }

    match cmd.output() {
        Ok(output) => {
            tracing::debug!("Command stdout: {}", String::from_utf8_lossy(&output.stdout));
            if !output.status.success() {
                tracing::error!(
                    status = ?output.status,
                    stderr = %String::from_utf8_lossy(&output.stderr),
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
