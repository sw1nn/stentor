use anyhow::{Context, Result};
use async_channel::Sender;
use clap::Parser;
use gtk4::prelude::*;
use gtk4::{Application, glib};
use libadwaita as adw;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
use daemon::{DaemonCommand, DaemonServer, MultiSlotHandler};
use dialog::{DestinationSlot, TranscriptionDialog, TranscriptionState};
use kitty::{find_claude_windows, list_kitty_windows, set_background_color, set_window_env_var};
use palette::Palette;
use source_mute::SourceMuteManager;
use transcription::Transcriber;

#[derive(Parser)]
#[command(name = "stentord")]
#[command(about = "Real-time transcription daemon", long_about = None)]
#[command(
    after_help = "Configuration can be set in $XDG_CONFIG_HOME/stentor/config.toml.\nCommand-line arguments override config file settings."
)]
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
    AutoSendText(String, usize), // Auto-send text to specified slot (text, slot_num)
    SetDestinations(Vec<DestinationSlot>), // Update destination slots
    StoreKittyWindowIds(Vec<u64>), // Store Kitty window IDs that had their background changed
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
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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
    tracing::info!("Loaded configuration: {:?}", config);
    tracing::info!(
        "Using silence_duration: {} seconds",
        config.silence_duration
    );

    // Log build-time compilation flags for debugging
    tracing::info!("Build-time flags:");
    tracing::info!("  Profile: {}", env!("BUILD_PROFILE"));
    tracing::info!("  Opt Level: {}", env!("BUILD_OPT_LEVEL"));
    tracing::info!("  Debug: {}", env!("BUILD_DEBUG"));
    tracing::info!("  CFLAGS: {}", env!("BUILD_CFLAGS"));
    tracing::info!("  CXXFLAGS: {}", env!("BUILD_CXXFLAGS"));
    tracing::info!("  LDFLAGS: {}", env!("BUILD_LDFLAGS"));
    tracing::info!("  RUSTFLAGS: {}", env!("BUILD_RUSTFLAGS"));
    tracing::info!(
        "  CARGO_PROFILE_RELEASE_LTO: {}",
        env!("BUILD_CARGO_PROFILE_RELEASE_LTO")
    );
    tracing::info!(
        "  CARGO_PROFILE_RELEASE_CODEGEN_UNITS: {}",
        env!("BUILD_CARGO_PROFILE_RELEASE_CODEGEN_UNITS")
    );

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
    let current_stop_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<RecordingCommand>>>> =
        Arc::new(Mutex::new(None));
    // Store the command slot to send to when Stop is called (Arc<AtomicU8> for lock-free access, u8::MAX = None)
    let auto_send_slot: Arc<AtomicU8> = Arc::new(AtomicU8::new(u8::MAX));
    // Track if a recording session is currently active (Arc<AtomicBool> for lock-free access from multiple threads)
    let recording_active: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Clone for GTK main loop
    let current_dialog_clone = Rc::clone(&current_dialog);
    let current_ui_tx_clone = Rc::clone(&current_ui_tx);
    let current_stop_tx_clone = Arc::clone(&current_stop_tx);
    let auto_send_slot_clone = Arc::clone(&auto_send_slot);
    let recording_active_clone = Arc::clone(&recording_active);
    let app_clone = app.clone();
    let config_clone = config.clone();
    let transcriber_clone = Arc::clone(&transcriber);

    // Setup GTK event loop integration with tokio
    glib::MainContext::default().spawn_local(async move {
        while let Some(command) = command_rx.recv().await {
            tracing::info!("Processing command: {:?}", command);

            match command {
                DaemonCommand::Start { unmute_source, source, multi_slot_handler } => {
                    // Atomically check if recording is active and set it to true if not
                    // This prevents race conditions where multiple Start commands arrive concurrently
                    if recording_active_clone
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        tracing::warn!("Recording already in progress, ignoring Start command");
                        // Bring existing dialog to front if it exists
                        let dialog_ref = current_dialog_clone.borrow();
                        if let Some(ref dialog) = *dialog_ref {
                            dialog.present();
                        }
                        continue;
                    }

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
                        let recording_active_for_ui = Arc::clone(&recording_active_clone);
                        let stop_tx_for_cleanup = Arc::clone(&current_stop_tx_clone);
                        let config_for_auto_send = config_clone.clone();
                        let config_for_reset = config_clone.clone();
                        let kitty_window_ids: Rc<RefCell<Vec<u64>>> = Rc::new(RefCell::new(Vec::new()));
                        let kitty_window_ids_clone = Rc::clone(&kitty_window_ids);
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
                                    UIMessage::SetDestinations(destinations) => {
                                        tracing::info!("Updating destinations with {} slots", destinations.len());
                                        dialog_for_updates.set_destinations(&destinations);
                                    }
                                    UIMessage::StoreKittyWindowIds(window_ids) => {
                                        tracing::info!("Storing {} Kitty window IDs for cleanup", window_ids.len());
                                        *kitty_window_ids.borrow_mut() = window_ids;
                                    }
                                    UIMessage::AutoSendText(text, dest_num) => {
                                        tracing::info!("Auto-sending text to destination {}: {}", dest_num, text);

                                        // Select command based on destination
                                        let cmd = if dest_num == 0 {
                                            config_for_auto_send.output_command.as_ref()
                                        } else {
                                            match dest_num {
                                                1 => config_for_auto_send.output_command_1.as_ref(),
                                                2 => config_for_auto_send.output_command_2.as_ref(),
                                                3 => config_for_auto_send.output_command_3.as_ref(),
                                                4 => config_for_auto_send.output_command_4.as_ref(),
                                                _ => None,
                                            }
                                        };

                                        if let Some(cmd_str) = cmd {
                                            execute_output_command(cmd_str, &text);
                                        }

                                        // Reset Kitty window background colors before closing
                                        let window_ids = kitty_window_ids_clone.borrow().clone();
                                        if !window_ids.is_empty() {
                                            let background_color = get_background_color(&config_for_reset);
                                            tracing::info!("Resetting background color for {} Kitty windows to {}", window_ids.len(), background_color);
                                            for window_id in window_ids {
                                                if let Err(e) = set_background_color(&background_color, window_id) {
                                                    tracing::warn!("Failed to reset background color for window {}: {}", window_id, e);
                                                } else {
                                                    tracing::debug!("Reset background color for window {}", window_id);
                                                }
                                            }
                                        }

                                        // Close dialog after auto-send
                                        tracing::info!("Closing dialog after auto-send");
                                        dialog_for_updates.close();
                                        *dialog_state_clone.borrow_mut() = None;
                                        *ui_tx_state_clone.borrow_mut() = None;
                                        // Clear recording active flag
                                        recording_active_for_ui.store(false, Ordering::Release);
                                        break;
                                    }
                                    UIMessage::Close => {
                                        tracing::info!("Closing dialog and cleaning up state");

                                        // Stop recording thread if it's still running
                                        let stop_tx_lock = stop_tx_for_cleanup.lock().unwrap();
                                        if let Some(ref tx) = *stop_tx_lock {
                                            tracing::info!("Sending stop command to recording thread");
                                            let _ = tx.send(RecordingCommand::Stop);
                                        }
                                        drop(stop_tx_lock);

                                        // Reset Kitty window background colors if any were set
                                        let window_ids = kitty_window_ids_clone.borrow().clone();
                                        if !window_ids.is_empty() {
                                            let background_color = get_background_color(&config_for_reset);
                                            tracing::info!("Resetting background color for {} Kitty windows to {}", window_ids.len(), background_color);
                                            for window_id in window_ids {
                                                if let Err(e) = set_background_color(&background_color, window_id) {
                                                    tracing::warn!("Failed to reset background color for window {}: {}", window_id, e);
                                                } else {
                                                    tracing::debug!("Reset background color for window {}", window_id);
                                                }
                                            }
                                        }

                                        dialog_for_updates.close();
                                        // Clean up state
                                        *dialog_state_clone.borrow_mut() = None;
                                        *ui_tx_state_clone.borrow_mut() = None;
                                        // Clear recording active flag
                                        recording_active_for_ui.store(false, Ordering::Release);
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
                                execute_output_command(cmd_str, &text);
                            }

                            // Close dialog
                            let _ = ui_tx_for_close.send_blocking(UIMessage::Close);
                        });

                        let ui_tx_for_cancel = ui_tx.clone();
                        dialog.set_on_cancel(move || {
                            tracing::info!("Cancelled");
                            let _ = ui_tx_for_cancel.send_blocking(UIMessage::Close);
                        });

                        let stop_tx_clone2 = Arc::clone(&current_stop_tx_clone);
                        let auto_send_clone2 = Arc::clone(&auto_send_slot_clone);
                        dialog.set_on_stop_and_send(move |dest_num| {
                            tracing::info!("Stop and send to slot {} requested", dest_num);

                            // Store the destination slot for auto-send
                            auto_send_clone2.store(dest_num as u8, Ordering::Release);

                            // Trigger stop to finalize transcription
                            let stop_tx_lock = stop_tx_clone2.lock().unwrap();
                            if let Some(ref tx) = *stop_tx_lock {
                                let _ = tx.send(RecordingCommand::Stop);
                            }
                        });

                        dialog.setup_key_handlers();

                        let ui_tx_for_close_handler = ui_tx.clone();
                        dialog.connect_close_handler(move || {
                            let _ = ui_tx_for_close_handler.send_blocking(UIMessage::Close);
                        });

                        // Start recording
                        dialog.update_state(TranscriptionState::Recording, "Listening...", 0.0);
                        dialog.present();

                        // Setup multi-slot handler in background if requested
                        if multi_slot_handler == MultiSlotHandler::Kitty {
                            // Immediately show inactive slots so Ctrl+1-4 labels are visible
                            let mut inactive_destinations = Vec::new();
                            for slot_num in 1..=4 {
                                inactive_destinations.push(DestinationSlot::inactive(slot_num));
                            }
                            let _ = ui_tx.send_blocking(UIMessage::SetDestinations(inactive_destinations));
                            tracing::info!("Initialized 4 inactive destination slots");

                            let ui_tx_for_kitty = ui_tx.clone();
                            thread::spawn(move || {
                                tracing::info!("Background: Starting Kitty window discovery");

                                match list_kitty_windows() {
                                    Ok(data) => {
                                        let claude_windows = find_claude_windows(&data);
                                        tracing::info!("Background: Found {} Claude windows", claude_windows.len());

                                        // Always build and send destination slots (even if empty) to update from initial state
                                        let palette = Palette::new("#1e1e2e");
                                        let mut destinations = Vec::new();
                                        let mut modified_window_ids = Vec::new();

                                        // Set colors and env vars, build destination slots for found windows
                                        for (i, window) in claude_windows.iter().enumerate().take(4) {
                                            let slot_num = i + 1;
                                            if let Some((_name, color_hex)) = palette.get_slot_color(slot_num) {
                                                // Set background color
                                                if let Err(e) = set_background_color(color_hex, window.id) {
                                                    tracing::warn!("Failed to set background color for window {}: {}", window.id, e);
                                                } else {
                                                    tracing::debug!("Set background color {} for window {}", color_hex, window.id);
                                                    modified_window_ids.push(window.id);
                                                }

                                                // Set environment variable
                                                let env_var_name = format!("STENTOR_SLOT_{}", slot_num);
                                                if let Err(e) = set_window_env_var(&env_var_name, "1", window.id) {
                                                    tracing::warn!("Failed to set env var {} for window {}: {}", env_var_name, window.id, e);
                                                } else {
                                                    tracing::debug!("Set env var {} for window {}", env_var_name, window.id);
                                                }

                                                // Create destination slot with label
                                                let label = window.cwd.split('/').last().unwrap_or(&window.cwd).to_string();
                                                destinations.push(DestinationSlot::with_label(slot_num, label, color_hex.to_string()));
                                            }
                                        }

                                        // Store window IDs for cleanup
                                        if !modified_window_ids.is_empty() {
                                            let _ = ui_tx_for_kitty.send_blocking(UIMessage::StoreKittyWindowIds(modified_window_ids));
                                        }

                                        // Fill remaining slots as inactive (greyed out)
                                        for i in claude_windows.len()..4 {
                                            let slot_num = i + 1;
                                            destinations.push(DestinationSlot::inactive(slot_num));
                                        }

                                        // Send updated destinations to UI
                                        tracing::info!("Background: Sending {} destination slots to UI ({} with labels)", destinations.len(), claude_windows.len());
                                        let _ = ui_tx_for_kitty.send_blocking(UIMessage::SetDestinations(destinations));
                                    }
                                    Err(e) => {
                                        tracing::warn!("Background: Failed to list Kitty windows: {}", e);
                                        // Empty slots are already showing from initial setup, no need to update
                                    }
                                }
                            });
                        }

                        // Note: recording_active flag was already set by compare_exchange above

                        // Start recording in background
                        let config_clone = config_clone.clone();
                        let transcriber_clone = Arc::clone(&transcriber_clone);
                        let ui_tx_for_recording = ui_tx.clone();
                        let stop_tx_storage = Arc::clone(&current_stop_tx_clone);
                        let auto_send_for_recording = Arc::clone(&auto_send_slot_clone);
                        let recording_active_for_thread = Arc::clone(&recording_active_clone);

                        thread::spawn(move || {
                            match start_recording_session(
                                config_clone,
                                transcriber_clone,
                                ui_tx_for_recording,
                                stop_tx_storage.clone(),
                                unmute_source,
                                source,
                                auto_send_for_recording.clone(),
                            ) {
                                Ok(_) => {
                                    tracing::info!("Recording session completed");
                                }
                                Err(e) => {
                                    tracing::error!("Recording session error: {}", e);
                                }
                            }
                            // Clear stop_tx and auto_send_slot when done
                            *stop_tx_storage.lock().unwrap() = None;
                            auto_send_for_recording.store(u8::MAX, Ordering::Release);
                            // Clear recording active flag
                            recording_active_for_thread.store(false, Ordering::Release);
                            tracing::info!("Recording thread finished, cleared recording_active flag");
                        });

                        *dialog_ref = Some(dialog);
                    } else {
                        // Dialog already exists, bring to front
                        if let Some(ref dialog) = *dialog_ref {
                            dialog.present();
                        }
                    }
                }
                DaemonCommand::Stop { command_slot } => {
                    // Store the command slot for auto-send after transcription
                    auto_send_slot_clone.store(command_slot as u8, Ordering::Release);

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
    auto_send_slot: Arc<AtomicU8>,
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
                tracing::debug!(
                    "VAD: state transition {:?} -> {:?}, silence_chunks={}, should_stop={}",
                    vad_state,
                    result.state,
                    silence_chunks,
                    result.should_stop
                );
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
                                "Transcribing... (press Escape or stop speaking to finish)"
                                    .to_string(),
                                rms as f64,
                            ));
                        } else {
                            // Update UI with audio level during transcription
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Processing,
                                "Transcribing... (press Escape or stop speaking to finish)"
                                    .to_string(),
                                rms as f64,
                            ));
                        }

                        // Continuous transcription - re-transcribe ALL audio for accuracy
                        if last_transcription.elapsed() >= transcription_interval
                            && !recorded_audio.is_empty()
                        {
                            tracing::info!(
                                "Re-transcribing all {} chunks for accuracy",
                                recorded_audio.len()
                            );
                            last_transcription = std::time::Instant::now();

                            // Clone all accumulated audio for transcription
                            let audio_flat: Vec<f32> =
                                recorded_audio.iter().flatten().copied().collect();
                            let transcriber_ref = Arc::clone(&transcriber_clone);
                            let ui_tx_transcribe = ui_tx_for_transcription.clone();
                            let text_accumulator = Arc::clone(&accumulated_text_clone);

                            // Spawn transcription in background to avoid blocking recording
                            thread::spawn(move || {
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
                                        let complete_text = full_text.lock().unwrap().clone();
                                        if !complete_text.is_empty() {
                                            // Replace accumulated text with the complete re-transcription
                                            *text_accumulator.lock().unwrap() =
                                                complete_text.clone();
                                            tracing::info!(
                                                "Complete transcription: '{}'",
                                                complete_text
                                            );
                                            let _ = ui_tx_transcribe.send_blocking(
                                                UIMessage::SetTextPreview(complete_text),
                                            );
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
                        if last_transcription.elapsed() >= transcription_interval
                            && !recorded_audio.is_empty()
                        {
                            tracing::info!(
                                "Re-transcribing all {} chunks during pause",
                                recorded_audio.len()
                            );
                            last_transcription = std::time::Instant::now();

                            // Clone all accumulated audio for transcription
                            let audio_flat: Vec<f32> =
                                recorded_audio.iter().flatten().copied().collect();
                            let transcriber_ref = Arc::clone(&transcriber_clone);
                            let ui_tx_transcribe = ui_tx_for_transcription.clone();
                            let text_accumulator = Arc::clone(&accumulated_text_clone);

                            // Spawn transcription in background to avoid blocking recording
                            thread::spawn(move || {
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
                                        let complete_text = full_text.lock().unwrap().clone();
                                        if !complete_text.is_empty() {
                                            // Replace accumulated text with the complete re-transcription
                                            *text_accumulator.lock().unwrap() =
                                                complete_text.clone();
                                            tracing::info!(
                                                "Complete transcription during pause: '{}'",
                                                complete_text
                                            );
                                            let _ = ui_tx_transcribe.send_blocking(
                                                UIMessage::SetTextPreview(complete_text),
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            "Continuous transcription during pause failed: {}",
                                            e
                                        );
                                    }
                                }
                            });
                        }

                        if result.should_stop {
                            tracing::info!(
                                "VAD detected end of speech (silence duration exceeded)"
                            );
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

    // Check if we should auto-send to a specific slot (atomically retrieve and clear)
    let slot_value = auto_send_slot.swap(u8::MAX, Ordering::AcqRel);
    let auto_slot = if slot_value == u8::MAX { None } else { Some(slot_value as usize) };

    if final_text.is_empty() {
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
                    if !cleaned.is_empty() {
                        // Auto-send or show for review
                        if let Some(slot) = auto_slot {
                            tracing::info!("Auto-sending to slot {}", slot);
                            let _ = ui_tx.send_blocking(UIMessage::AutoSendText(cleaned, slot));
                        } else {
                            let _ = ui_tx.send_blocking(UIMessage::SetText(cleaned.clone()));
                            let _ = ui_tx.send_blocking(UIMessage::UpdateState(
                                TranscriptionState::Reviewing,
                                "Ready to send".to_string(),
                                0.0,
                            ));
                        }
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

    // Auto-send or show final result in dialog for review
    if let Some(slot) = auto_slot {
        tracing::info!("Auto-sending to slot {}", slot);
        let _ = ui_tx.send_blocking(UIMessage::AutoSendText(final_text, slot));
    } else {
        let _ = ui_tx.send_blocking(UIMessage::SetText(final_text));
        let _ = ui_tx.send_blocking(UIMessage::UpdateState(
            TranscriptionState::Reviewing,
            "Ready to send".to_string(),
            0.0,
        ));
    }

    Ok(())
}

fn execute_output_command(command_template: &str, text: &str) {
    tracing::info!(transcription = text, command_template, "Executing command");

    // Pass transcription via environment variable to prevent shell injection
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command_template)
        .env("TRANSCRIPTION", text);

    match cmd.output() {
        Ok(output) => {
            tracing::debug!(
                "Command stdout: {}",
                String::from_utf8_lossy(&output.stdout)
            );
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

fn get_background_color(config: &Config) -> String {
    // If a custom command is configured, try to use it
    if let Some(cmd) = &config.kitty_background_color_cmd {
        tracing::info!("Retrieving background color using command: {}", cmd);

        match std::process::Command::new("sh").arg("-c").arg(cmd).output() {
            Ok(output) => {
                if output.status.success() {
                    let color = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if !color.is_empty() {
                        tracing::info!("Retrieved background color: {}", color);
                        return color;
                    } else {
                        tracing::warn!("Command returned empty output, using default");
                    }
                } else {
                    tracing::warn!(
                        "Command failed with status {:?}: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to execute background color retrieval command: {}",
                    e
                );
            }
        }
    }

    // Fall back to default
    tracing::info!("Using default background color: #1e1e2e");
    "#1e1e2e".to_string()
}
