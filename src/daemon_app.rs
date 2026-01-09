use anyhow::Result;
use async_channel::Sender;
use gtk4::prelude::*;
use gtk4::{Application, glib};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use tokio::sync::mpsc as tokio_mpsc;

use crate::audio::{AudioRecorder, RecordingCommand};
use crate::config::{Config, ConfigFile};
use crate::defaults;
use crate::daemon::{DaemonCommand, DaemonServer, MultiSlotHandler};
use crate::dialog::{TranscriptionDialog, TranscriptionState};
use crate::kitty;
use crate::multi_slot::{HandlerUIMessage, MultiSlotHandler as MultiSlotHandlerTrait};
use crate::recording::{UIMessage, execute_output_command, start_recording_session};
use crate::transcription::Transcriber;

pub async fn run(
    config: Arc<Config>,
    config_file: Arc<ConfigFile>,
    transcriber: Arc<Transcriber>,
    socket_path: PathBuf,
) -> Result<()> {
    // Create daemon server
    let mut server = DaemonServer::new(socket_path)?;
    server.bind().await?;

    // Channel for daemon commands
    let (command_tx, mut command_rx) =
        tokio_mpsc::channel::<DaemonCommand>(defaults::COMMAND_CHANNEL_CAPACITY);

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

    // Application state (create RefCell before startup handler)
    // Use Rc<RefCell<>> for GTK widgets since all GTK operations happen on the main thread
    let current_dialog: Rc<RefCell<Option<TranscriptionDialog>>> = Rc::new(RefCell::new(None));

    // Create dialog in startup handler to avoid GTK warning
    let current_dialog_for_startup = Rc::clone(&current_dialog);
    app.connect_startup(move |app| {
        // Create the dialog once at startup (hidden)
        let dialog = TranscriptionDialog::new(app);
        dialog.hide();
        *current_dialog_for_startup.borrow_mut() = Some(dialog);
        tracing::info!("Created persistent dialog (hidden)");
    });

    // Add a dummy activate handler to suppress GTK warning
    app.connect_activate(|_| {
        // This daemon is socket-driven, not activation-driven
        // This handler exists only to suppress the GTK warning
    });

    // Hold the application to keep it running even when no windows are shown
    let _hold_guard = app.hold();
    let current_ui_tx: Rc<RefCell<Option<Sender<UIMessage>>>> = Rc::new(RefCell::new(None));
    // Keep Arc<Mutex<>> for stop_tx since it's shared with background threads
    let current_stop_tx: Arc<Mutex<Option<std::sync::mpsc::Sender<RecordingCommand>>>> =
        Arc::new(Mutex::new(None));
    // Store the command slots to send to when Stop is called (Arc<Mutex<Vec<usize>>> to support multiple slots)
    let auto_send_slots: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    // Track if a recording session is currently active (Arc<AtomicBool> for lock-free access from multiple threads)
    let recording_active: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Clone for GTK main loop
    let current_dialog_clone = Rc::clone(&current_dialog);
    let current_ui_tx_clone = Rc::clone(&current_ui_tx);
    let current_stop_tx_clone = Arc::clone(&current_stop_tx);
    let auto_send_slots_clone = Arc::clone(&auto_send_slots);
    let recording_active_clone = Arc::clone(&recording_active);
    let app_clone = app.clone();
    let config_clone = Arc::clone(&config);
    let config_file_clone = Arc::clone(&config_file);
    let transcriber_clone = Arc::clone(&transcriber);

    // Setup GTK event loop integration with tokio
    glib::MainContext::default().spawn_local(async move {
        while let Some(command) = command_rx.recv().await {
            tracing::info!("Processing command: {:?}", command);

            use DaemonCommand::*;
            match command {
                Start { unmute_source, source, multi_slot_handler } => {
                    // Atomically check-and-set recording_active to prevent race conditions.
                    // If another Start command comes in while we're setting up, it will fail
                    // the compare_exchange and be rejected.
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
                    // recording_active is now true - we own the recording session

                    // Reset dialog state and set source info before cloning
                    {
                        let mut dialog_ref = current_dialog_clone.borrow_mut();
                        let dialog = dialog_ref.as_mut().expect("Dialog should always exist");
                        dialog.reset();

                        // Get source info and set it while we have mutable borrow
                        let source_name = match AudioRecorder::new(16000, source.clone(), config_clone.chunk_size) {
                            Ok(recorder) => recorder.get_device_name().unwrap_or_else(|_| "Default".to_string()),
                            Err(_) => "Default".to_string(),
                        };
                        dialog.set_source_info(&source_name);
                    }

                        // Create UI update channel with backpressure
                        // Bounded to prevent OOM if UI thread blocks
                        let (ui_tx, ui_rx) =
                            async_channel::bounded::<UIMessage>(defaults::UI_MESSAGE_CHANNEL_CAPACITY);
                        *current_ui_tx_clone.borrow_mut() = Some(ui_tx.clone());

                        // Setup multi-slot handler in background if requested
                        // Store handler for cleanup later
                        let handler: Option<Arc<dyn MultiSlotHandlerTrait>> = match multi_slot_handler {
                            MultiSlotHandler::Kitty => {
                                // Create a channel for handler messages
                                let (handler_tx, handler_rx) =
                                    async_channel::bounded::<HandlerUIMessage>(defaults::HANDLER_CHANNEL_CAPACITY);

                                // Bridge handler messages to UI messages
                                let ui_tx_for_bridge = ui_tx.clone();
                                glib::MainContext::default().spawn_local(async move {
                                    while let Ok(msg) = handler_rx.recv().await {
                                        use HandlerUIMessage::*;
                                        match msg {
                                            SetDestinations(destinations) => {
                                                let _ = ui_tx_for_bridge.send_blocking(UIMessage::SetDestinations(destinations));
                                            }
                                            StoreWindowIds(window_ids) => {
                                                let _ = ui_tx_for_bridge.send_blocking(UIMessage::StoreHandlerWindowIds(window_ids));
                                            }
                                        }
                                    }
                                });

                                // Create and setup the Kitty handler
                                let kitty_handler = kitty::KittyMultiSlotHandler::new(config_file_clone.kitty.clone());
                                if let Err(e) = kitty_handler.setup(handler_tx) {
                                    tracing::error!("Failed to setup Kitty multi-slot handler: {}", e);
                                }
                                Some(Arc::new(kitty_handler) as Arc<dyn MultiSlotHandlerTrait>)
                            }
                            MultiSlotHandler::None => None,
                        };

                        // Determine which output command to use based on handler type
                        let output_command = match multi_slot_handler {
                            MultiSlotHandler::Kitty => config_file_clone.kitty.output_command.clone(),
                            MultiSlotHandler::None => config_clone.output_command.clone(),
                        };

                        // Setup UI message receiver
                        // Clone dialog for async closure (needs 'static lifetime)
                        let dialog_for_updates = current_dialog_clone.borrow().as_ref().expect("Dialog should always exist").clone();
                        let ui_tx_state_clone = Rc::clone(&current_ui_tx_clone);
                        let recording_active_for_ui = Arc::clone(&recording_active_clone);
                        let stop_tx_for_cleanup = Arc::clone(&current_stop_tx_clone);
                        let output_command_for_auto_send = output_command.clone();
                        // Window IDs are set once when Kitty windows are discovered, then only read on escape/send.
                        // Arc<Mutex> has negligible overhead: one write at discovery, few reads at user action,
                        // and no hot path contention. The Mutex cost is nothing compared to the actual cleanup
                        // work (IPC calls to reset Kitty window colors).
                        let kitty_window_ids: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
                        let kitty_window_ids_for_callbacks = Arc::clone(&kitty_window_ids);
                        let handler_for_close = handler.clone();
                        glib::MainContext::default().spawn_local(async move {
                            while let Ok(msg) = ui_rx.recv().await {
                                tracing::trace!("UI message received: {:?}", msg);
                                use UIMessage::*;
                                match msg {
                                    UpdateState(state, text, level) => {
                                        dialog_for_updates.update_state(state, &text, level);
                                    }
                                    SetText(text) => {
                                        dialog_for_updates.set_transcribed_text(&text);
                                    }
                                    SetTextPreview(text) => {
                                        dialog_for_updates.set_text_preview(&text);
                                    }
                                    SetConfirmedAndPreview(confirmed, preview) => {
                                        dialog_for_updates
                                            .set_confirmed_and_preview(&confirmed, &preview);
                                    }
                                    SetDestinations(destinations) => {
                                        tracing::info!("Updating destinations with {} slots", destinations.len());
                                        dialog_for_updates.set_destinations(&destinations);
                                    }
                                    StoreHandlerWindowIds(window_ids) => {
                                        tracing::info!("Storing {} handler window IDs for cleanup", window_ids.len());
                                        *kitty_window_ids.lock() = window_ids;
                                    }
                                    CloseImmediately => {
                                        tracing::info!("Hiding dialog immediately (background processing will continue)");
                                        dialog_for_updates.hide();
                                        // Don't break - wait for AutoSendText to do cleanup and close
                                    }
                                    AutoSendText(text, dest_num) => {
                                        tracing::info!("AutoSendText received: dest_num={}, text='{}'", dest_num, text);

                                        // Execute output command in background
                                        let cmd_str = output_command_for_auto_send.clone();
                                        let extra_env_vars = handler_for_close
                                            .as_ref()
                                            .map(|h| h.output_env_vars(dest_num))
                                            .unwrap_or_default();
                                        tracing::info!("Output command configured: {:?}", cmd_str);
                                        std::thread::spawn(move || {
                                            tracing::info!("Background thread started for slot {}", dest_num);
                                            if let Some(ref cmd) = cmd_str {
                                                tracing::info!("Executing output command for slot {}: {}", dest_num, cmd);
                                                execute_output_command(cmd, &text, dest_num, &extra_env_vars);
                                                tracing::info!("Output command execution complete for slot {}", dest_num);
                                            } else {
                                                tracing::warn!("No output command configured for sending text");
                                            }
                                            tracing::info!("Background processing complete for slot {}", dest_num);
                                        });
                                        // Don't break - continue processing more AutoSendText messages
                                    }
                                    Close => {
                                        tracing::info!("Hiding dialog and cleaning up state");

                                        // Stop recording thread if it's still running
                                        let stop_tx_lock = stop_tx_for_cleanup.lock();
                                        if let Some(ref tx) = *stop_tx_lock {
                                            tracing::info!("Sending stop command to recording thread");
                                            let _ = tx.send(RecordingCommand::Stop);
                                        }
                                        drop(stop_tx_lock);

                                        // Cleanup handler if present
                                        if let Some(ref h) = handler_for_close {
                                            let window_ids = kitty_window_ids.lock().clone();
                                            if let Err(e) = h.cleanup(window_ids) {
                                                tracing::error!("Handler cleanup failed: {}", e);
                                            }
                                        }

                                        // Hide dialog but don't destroy - it will be reused for next session
                                        dialog_for_updates.hide();
                                        // Clear session state (dialog persists for reuse)
                                        *ui_tx_state_clone.borrow_mut() = None;
                                        // Clear recording active flag
                                        recording_active_for_ui.store(false, Ordering::Release);
                                        // Stop processing messages
                                        break;
                                    }
                                }
                            }
                        });

                        // Setup callbacks and present dialog
                        // Borrow dialog mutably for callback setup
                        {
                            let mut dialog_ref = current_dialog_clone.borrow_mut();
                            let dialog = dialog_ref.as_mut().expect("Dialog should always exist");

                            let stop_tx_clone = Arc::clone(&current_stop_tx_clone);
                            let ui_tx_for_manual_stop = ui_tx.clone();
                            let handler_for_manual_stop = handler.clone();
                            let kitty_window_ids_for_manual_stop = Arc::clone(&kitty_window_ids_for_callbacks);
                            dialog.set_on_manual_stop(move || {
                                tracing::info!("Manual stop requested - closing immediately");
                                // Hide dialog immediately for responsive UX
                                let _ = ui_tx_for_manual_stop.send_blocking(UIMessage::CloseImmediately);

                                // Clone window_ids now (at invocation time, not setup time)
                                let window_ids = kitty_window_ids_for_manual_stop.lock().clone();

                                // Do cleanup in background
                                let stop_tx = Arc::clone(&stop_tx_clone);
                                let handler = handler_for_manual_stop.clone();
                                let ui_tx_close = ui_tx_for_manual_stop.clone();
                                std::thread::spawn(move || {
                                    // Stop recording thread
                                    let stop_tx_lock = stop_tx.lock();
                                    if let Some(ref tx) = *stop_tx_lock {
                                        let _ = tx.send(RecordingCommand::Stop);
                                    }
                                    drop(stop_tx_lock);

                                    // Cleanup handler (e.g., reset Kitty window colors)
                                    if let Some(ref h) = handler
                                        && let Err(e) = h.cleanup(window_ids)
                                    {
                                        tracing::error!("Handler cleanup failed: {}", e);
                                    }

                                    // Finalize close
                                    let _ = ui_tx_close.send_blocking(UIMessage::Close);
                                });
                            });

                            let output_command_for_send = output_command.clone();
                            let ui_tx_for_close = ui_tx.clone();
                            let handler_for_send = handler.clone();
                            let kitty_window_ids_for_send = Arc::clone(&kitty_window_ids_for_callbacks);
                            dialog.set_on_send_text(move |text, dest_num| {
                                tracing::info!("Sending text to destination {}: {}", dest_num, text);

                                // Close dialog immediately for better UX
                                tracing::info!("Closing dialog immediately (output command continues in background)");
                                let _ = ui_tx_for_close.send_blocking(UIMessage::CloseImmediately);

                                // Clone window_ids now (at invocation time, not setup time)
                                let window_ids = kitty_window_ids_for_send.lock().clone();

                                // Execute output command and cleanup in background
                                let cmd_str = output_command_for_send.clone();
                                let extra_env_vars = handler_for_send
                                    .as_ref()
                                    .map(|h| h.output_env_vars(dest_num))
                                    .unwrap_or_default();
                                let handler = handler_for_send.clone();
                                let ui_tx_close = ui_tx_for_close.clone();
                                std::thread::spawn(move || {
                                    if let Some(ref cmd) = cmd_str {
                                        execute_output_command(cmd, &text, dest_num, &extra_env_vars);
                                    } else {
                                        tracing::warn!("No output command configured for sending text");
                                    }

                                    // Cleanup handler in background
                                    if let Some(ref h) = handler
                                        && let Err(e) = h.cleanup(window_ids)
                                    {
                                        tracing::error!("Handler cleanup failed: {}", e);
                                    }

                                    // Send final Close after background processing
                                    let _ = ui_tx_close.send_blocking(UIMessage::Close);
                                });
                            });

                            let ui_tx_for_cancel = ui_tx.clone();
                            let handler_for_cancel = handler.clone();
                            let kitty_window_ids_for_cancel = Arc::clone(&kitty_window_ids_for_callbacks);
                            let stop_tx_for_cancel = Arc::clone(&current_stop_tx_clone);
                            dialog.set_on_cancel(move || {
                                tracing::info!("Cancelled - closing immediately");
                                // Hide dialog immediately for responsive UX
                                let _ = ui_tx_for_cancel.send_blocking(UIMessage::CloseImmediately);

                                // Clone window_ids now (at invocation time, not setup time)
                                let window_ids = kitty_window_ids_for_cancel.lock().clone();

                                // Do cleanup in background
                                let handler = handler_for_cancel.clone();
                                let stop_tx = Arc::clone(&stop_tx_for_cancel);
                                let ui_tx_close = ui_tx_for_cancel.clone();
                                std::thread::spawn(move || {
                                    // Stop recording thread if running
                                    let stop_tx_lock = stop_tx.lock();
                                    if let Some(ref tx) = *stop_tx_lock {
                                        let _ = tx.send(RecordingCommand::Stop);
                                    }
                                    drop(stop_tx_lock);

                                    // Cleanup handler (e.g., reset Kitty window colors)
                                    if let Some(ref h) = handler
                                        && let Err(e) = h.cleanup(window_ids)
                                    {
                                        tracing::error!("Handler cleanup failed: {}", e);
                                    }

                                    // Finalize close
                                    let _ = ui_tx_close.send_blocking(UIMessage::Close);
                                });
                            });

                            let stop_tx_clone2 = Arc::clone(&current_stop_tx_clone);
                            let auto_send_slots_clone2 = Arc::clone(&auto_send_slots_clone);
                            let ui_tx_for_stop_and_send = ui_tx.clone();
                            dialog.set_on_stop_and_send(move |dest_num| {
                                tracing::info!("Stop and send to slot {} requested", dest_num);

                                // Close dialog immediately for better UX (cleanup happens after transcription)
                                tracing::info!("Closing dialog immediately (transcription continues in background)");
                                let _ = ui_tx_for_stop_and_send.send_blocking(UIMessage::CloseImmediately);

                                // Store the destination slot for auto-send
                                auto_send_slots_clone2.lock().push(dest_num);

                                // Trigger stop to finalize transcription (continues in background)
                                let stop_tx_lock = stop_tx_clone2.lock();
                                if let Some(ref tx) = *stop_tx_lock {
                                    let _ = tx.send(RecordingCommand::Stop);
                                }
                            });

                            dialog.setup_key_handlers();
                            dialog.setup_destination_click_handlers();

                            let ui_tx_for_close_handler = ui_tx.clone();
                            dialog.connect_close_handler(move || {
                                let _ = ui_tx_for_close_handler.send_blocking(UIMessage::Close);
                            });

                            // Start recording
                            dialog.update_state(TranscriptionState::Recording, "Listening...", 0.0);
                            dialog.present();
                        } // Drop mutable borrow

                        // Start recording in background
                        let config_clone = Arc::clone(&config_clone);
                        let transcriber_clone = Arc::clone(&transcriber_clone);
                        let ui_tx_for_recording = ui_tx.clone();
                        let stop_tx_storage = Arc::clone(&current_stop_tx_clone);
                        let auto_send_for_recording = Arc::clone(&auto_send_slots_clone);
                        let recording_active_for_thread = Arc::clone(&recording_active_clone);

                        // recording_active was set atomically at the start via compare_exchange

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
                            // Clear stop_tx and auto_send_slots when done
                            *stop_tx_storage.lock() = None;
                            auto_send_for_recording.lock().clear();
                            // Clear recording active flag
                            recording_active_for_thread.store(false, Ordering::Release);
                            tracing::info!("Recording thread finished, cleared recording_active flag");
                        });
                }
                Stop { command_slot } => {
                    // Store the command slot for auto-send after transcription
                    auto_send_slots_clone.lock().push(command_slot);

                    // Trigger manual stop
                    let stop_tx_lock = current_stop_tx_clone.lock();
                    if let Some(ref tx) = *stop_tx_lock {
                        let _ = tx.send(RecordingCommand::Stop);
                    }
                }
                Status => {
                    tracing::info!("Status: running");
                }
                Quit => {
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
