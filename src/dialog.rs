use gtk4::prelude::*;
use gtk4::{gdk, glib, Application, ApplicationWindow, Box, Label, LevelBar, Orientation, Spinner, TextView, ScrolledWindow};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionState {
    Idle,
    Recording,
    Processing,
    Reviewing,
    #[allow(dead_code)]
    Typing,
    Error,
}

#[derive(Clone)]
pub struct TranscriptionDialog {
    window: ApplicationWindow,
    state: Arc<Mutex<TranscriptionState>>,
    spinner: Spinner,
    status_label: Label,
    mic_label: Label,
    level_bar: LevelBar,
    text_view: TextView,
    scrolled: ScrolledWindow,
    text_preview: Label,

    // Callbacks
    on_manual_stop: Option<Arc<dyn Fn() + Send + Sync>>,
    on_send_text: Option<Arc<dyn Fn(String) + Send + Sync>>,
    on_cancel: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl TranscriptionDialog {
    pub fn new(app: &Application) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Transcription")
            .default_width(700)
            .default_height(180)
            .resizable(false)
            .build();

        // Main content box
        let main_box = Box::new(Orientation::Vertical, 0);
        main_box.set_margin_top(0);
        main_box.set_margin_bottom(0);
        main_box.set_margin_start(0);
        main_box.set_margin_end(0);

        // Audio level indicator at the very top (full width)
        let level_bar = LevelBar::new();
        level_bar.set_min_value(0.0);
        level_bar.set_max_value(0.1);
        level_bar.set_value(0.0);
        level_bar.set_visible(true);
        level_bar.set_size_request(-1, 2);  // Full width, 2px height
        level_bar.set_vexpand(false);
        level_bar.set_valign(gtk4::Align::Start);
        main_box.append(&level_bar);

        // Content area with padding
        let content_box = Box::new(Orientation::Vertical, 10);
        content_box.set_margin_top(15);
        content_box.set_margin_bottom(10);
        content_box.set_margin_start(20);
        content_box.set_margin_end(20);

        // Text preview label (shown during recording/processing)
        let text_preview = Label::new(None);
        text_preview.set_wrap(true);
        text_preview.set_xalign(0.0);  // Left align
        text_preview.set_markup("<span foreground='#888888'>Listening...</span>");
        text_preview.set_vexpand(true);
        text_preview.set_valign(gtk4::Align::Start);
        content_box.append(&text_preview);

        // Editable text view (shown during review, hidden otherwise)
        let text_view = TextView::new();
        text_view.set_wrap_mode(gtk4::WrapMode::WordChar);
        text_view.set_left_margin(10);
        text_view.set_right_margin(10);
        text_view.set_top_margin(10);
        text_view.set_bottom_margin(10);

        // Put text view in a scrolled window
        let scrolled = ScrolledWindow::new();
        scrolled.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Automatic);
        scrolled.set_min_content_height(80);
        scrolled.set_vexpand(true);
        scrolled.set_child(Some(&text_view));
        scrolled.set_visible(false);
        content_box.append(&scrolled);

        main_box.append(&content_box);

        // Status bar at the bottom
        let status_box = Box::new(Orientation::Horizontal, 10);
        status_box.set_margin_top(5);
        status_box.set_margin_bottom(8);
        status_box.set_margin_start(20);
        status_box.set_margin_end(20);

        // Spinner (small, in status bar)
        let spinner = Spinner::new();
        spinner.set_size_request(16, 16);
        status_box.append(&spinner);

        // Status label (small text in status bar)
        let status_label = Label::new(None);
        status_label.set_markup("<small>Initializing...</small>");
        status_label.set_xalign(0.0);  // Left align
        status_box.append(&status_label);

        // Microphone info label (right-aligned in status bar)
        let mic_label = Label::new(None);
        mic_label.set_markup("<small><span foreground='#888888'>Detecting source...</span></small>");
        mic_label.set_xalign(1.0);  // Right align
        mic_label.set_hexpand(true);
        status_box.append(&mic_label);

        main_box.append(&status_box);

        window.set_child(Some(&main_box));

        let dialog = Self {
            window,
            state: Arc::new(Mutex::new(TranscriptionState::Idle)),
            spinner,
            status_label,
            mic_label,
            level_bar,
            text_view,
            scrolled,
            text_preview,
            on_manual_stop: None,
            on_send_text: None,
            on_cancel: None,
        };

        dialog
    }

    pub fn connect_close_handler<F>(&self, on_close: F)
    where
        F: Fn() + 'static,
    {
        let window = self.window.clone();
        window.connect_close_request(move |_| {
            log::info!("Window close requested");
            on_close();
            glib::Propagation::Proceed
        });
    }

    pub fn set_on_manual_stop<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_manual_stop = Some(Arc::new(callback));
    }

    pub fn set_on_send_text<F>(&mut self, callback: F)
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.on_send_text = Some(Arc::new(callback));
    }

    pub fn set_on_cancel<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_cancel = Some(Arc::new(callback));
    }

    pub fn setup_key_handlers(&self) {
        let state = Arc::clone(&self.state);
        let on_manual_stop = self.on_manual_stop.clone();
        let on_manual_stop_clone = on_manual_stop.clone();
        let on_send_text = self.on_send_text.clone();
        let on_cancel = self.on_cancel.clone();
        let text_view = self.text_view.clone();
        let window = self.window.clone();

        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
            let current_state = *state.lock().unwrap();

            // Escape key handling
            if keyval == gdk::Key::Escape {
                match current_state {
                    TranscriptionState::Recording | TranscriptionState::Processing => {
                        // Stop recording/transcribing and go to editing view
                        if let Some(ref callback) = on_manual_stop {
                            callback();
                        }
                        return glib::Propagation::Stop;
                    }
                    TranscriptionState::Reviewing => {
                        // Cancel without sending
                        if let Some(ref callback) = on_cancel {
                            callback();
                        }
                        return glib::Propagation::Stop;
                    }
                    _ => {
                        window.close();
                        return glib::Propagation::Stop;
                    }
                }
            }

            // Ctrl+Enter key handling - send text when reviewing
            if (keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter)
                && modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                && current_state == TranscriptionState::Reviewing
            {
                if let Some(ref callback) = on_send_text {
                    let buffer = text_view.buffer();
                    let text = buffer.text(
                        &buffer.start_iter(),
                        &buffer.end_iter(),
                        false,
                    );
                    callback(text.to_string());
                }
                return glib::Propagation::Stop;
            }

            glib::Propagation::Proceed
        });

        self.window.add_controller(key_controller);

        // Also add key controller to text view
        let state = Arc::clone(&self.state);
        let on_send_text = self.on_send_text.clone();
        let on_cancel = self.on_cancel.clone();
        let text_view_clone = self.text_view.clone();

        let text_key_controller = gtk4::EventControllerKey::new();
        text_key_controller.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
            let current_state = *state.lock().unwrap();

            // Escape key handling in text view
            if keyval == gdk::Key::Escape {
                match current_state {
                    TranscriptionState::Recording | TranscriptionState::Processing => {
                        // Stop recording/transcribing and go to editing view
                        if let Some(ref callback) = on_manual_stop_clone {
                            callback();
                        }
                        return glib::Propagation::Stop;
                    }
                    TranscriptionState::Reviewing => {
                        if let Some(ref callback) = on_cancel {
                            callback();
                        }
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }

            // Ctrl+Enter in text view
            if (keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter)
                && modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                && current_state == TranscriptionState::Reviewing
            {
                if let Some(ref callback) = on_send_text {
                    let buffer = text_view_clone.buffer();
                    let text = buffer.text(
                        &buffer.start_iter(),
                        &buffer.end_iter(),
                        false,
                    );
                    callback(text.to_string());
                }
                return glib::Propagation::Stop;
            }

            glib::Propagation::Proceed
        });

        self.text_view.add_controller(text_key_controller);
    }

    pub fn update_state(&self, state: TranscriptionState, message: &str, level: f64) {
        *self.state.lock().unwrap() = state;

        match state {
            TranscriptionState::Recording => {
                self.spinner.start();
                self.status_label.set_markup(&format!("<small>{}</small>", message));
                self.level_bar.set_visible(true);
                self.level_bar.set_value(level.min(0.1));
                log::debug!("Level bar updated: {}", level);
                self.text_preview.set_visible(true);
                self.scrolled.set_visible(false);
            }
            TranscriptionState::Processing => {
                self.spinner.start();
                self.status_label.set_markup(&format!("<small>⚙️ {}</small>", message));
                self.level_bar.set_visible(true);
                self.level_bar.set_value(level.min(0.1));
                self.text_preview.set_visible(true);  // Show text preview during processing
                self.scrolled.set_visible(false);  // Hide editable text view during processing
            }
            TranscriptionState::Reviewing => {
                self.spinner.stop();
                self.status_label.set_markup(&format!(
                    "<small>✓ {} • <span foreground='#888888'>Ctrl+Enter to send • Escape to cancel</span></small>",
                    message
                ));
                self.level_bar.set_visible(false);
                self.text_preview.set_visible(false);
                self.scrolled.set_visible(true);
                self.text_view.set_editable(true);  // Editable during reviewing
            }
            TranscriptionState::Typing => {
                self.spinner.start();
                self.status_label.set_markup(&format!("<small>⌨️ {}</small>", message));
                self.level_bar.set_visible(false);
                self.text_preview.set_visible(false);
                self.scrolled.set_visible(false);
            }
            TranscriptionState::Error => {
                self.spinner.stop();
                self.status_label.set_markup(&format!("<small>❌ {}</small>", message));
                self.level_bar.set_visible(false);
            }
            TranscriptionState::Idle => {
                self.spinner.stop();
                self.level_bar.set_visible(false);
            }
        }
    }

    pub fn set_source_info(&self, device_name: &str) {
        // Check if device name starts with "default"
        let is_default = device_name.to_lowercase().starts_with("default");

        // Extract just the description part if it's formatted as "default (Description)"
        let display_name = if is_default {
            if let Some(start) = device_name.find('(') {
                if let Some(end) = device_name.find(')') {
                    if start < end {
                        &device_name[start + 1..end]
                    } else {
                        device_name
                    }
                } else {
                    device_name
                }
            } else {
                device_name
            }
        } else {
            device_name
        };

        // Use grey color for default devices, white for explicitly chosen ones
        let color = if is_default { "#888888" } else { "#FFFFFF" };

        self.mic_label.set_markup(&format!(
            "<small><span foreground='{}'>🎙️ {}</span></small>",
            color, display_name
        ));
    }

    pub fn set_text_preview(&self, text: &str) {
        log::info!("Setting text preview: '{}'", text);
        self.text_preview.set_text(text);
    }

    pub fn set_transcribed_text(&self, text: &str) {
        let buffer = self.text_view.buffer();
        buffer.set_text(text);
    }

    #[allow(dead_code)]
    pub fn set_text_editable(&self, editable: bool) {
        self.text_view.set_editable(editable);
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn close(&self) {
        self.window.close();
    }

    #[allow(dead_code)]
    pub fn window(&self) -> &ApplicationWindow {
        &self.window
    }
}
