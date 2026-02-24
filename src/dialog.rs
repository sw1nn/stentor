use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box, Button, DeleteType, Label, LevelBar, MovementStep,
    Orientation, ScrolledWindow, Spinner, TextView, gdk, glib,
};
use parking_lot::Mutex;
use std::sync::Arc;

/// Helper function to send transcription to all active slots
fn handle_send_to_all(
    current_state: TranscriptionState,
    destinations: &Arc<Mutex<Vec<DestinationSlot>>>,
    text_view: &TextView,
    on_stop_and_send: Option<&Arc<dyn Fn(usize) + Send + Sync>>,
    on_send_text: Option<&Arc<dyn Fn(String, usize) + Send + Sync>>,
) {
    // Get active destinations (those with non-empty labels)
    let dest_list = destinations.lock();
    let active_destinations: Vec<_> = dest_list.iter().filter(|d| !d.label.is_empty()).collect();
    let is_multi_slot = !active_destinations.is_empty();

    use TranscriptionState::*;
    match current_state {
        Recording | Processing => {
            // Stop recording and send to all active slots (or default slot 0)
            if let Some(callback) = on_stop_and_send {
                if is_multi_slot {
                    for dest in active_destinations {
                        tracing::info!("Alt+Enter: sending to slot {}", dest.slot_num);
                        callback(dest.slot_num);
                    }
                } else {
                    callback(0);
                }
            }
        }
        Reviewing => {
            // Send text to all active slots (or default slot 0)
            if let Some(callback) = on_send_text {
                let buffer = text_view.buffer();
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();

                if is_multi_slot {
                    for dest in active_destinations {
                        tracing::info!(
                            "Alt+Enter: sending to slot {} with text: '{}'",
                            dest.slot_num,
                            text
                        );
                        callback(text.clone(), dest.slot_num);
                    }
                } else {
                    callback(text, 0);
                }
            }
        }
        _ => {}
    }
}

/// Helper function to send transcription to a specific slot
fn handle_send_to_slot(
    current_state: TranscriptionState,
    slot_num: usize,
    text_view: &TextView,
    on_stop_and_send: Option<&Arc<dyn Fn(usize) + Send + Sync>>,
    on_send_text: Option<&Arc<dyn Fn(String, usize) + Send + Sync>>,
) {
    use TranscriptionState::*;
    match current_state {
        Recording | Processing => {
            // Stop recording and send to specific slot
            if let Some(callback) = on_stop_and_send {
                callback(slot_num);
            }
        }
        Reviewing => {
            // Send text to specific slot
            if let Some(callback) = on_send_text {
                let buffer = text_view.buffer();
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string();
                callback(text, slot_num);
            }
        }
        _ => {}
    }
}

fn slot_unicode_char(slot_num: usize) -> char {
    match slot_num {
        1 => '\u{F03A5}', // 󰎥
        2 => '\u{F03A8}', // 󰎨
        3 => '\u{F03AB}', // 󰎫
        4 => '\u{F03B2}', // 󰎲
        5 => '\u{F03AF}', // 󰎯
        6 => '\u{F03B4}', // 󰎴
        7 => '\u{F03B7}', // 󰎷
        8 => '\u{F03BA}', // 󰎺
        _ => '?',
    }
}

#[derive(Debug, Clone)]
pub struct DestinationSlot {
    pub slot_num: usize,
    pub label: String,
    pub color_hex: String,
}

impl DestinationSlot {
    pub fn inactive(slot_num: usize) -> Self {
        Self {
            slot_num,
            label: String::new(), // Empty label for inactive slots
            color_hex: "transparent".to_string(), // Transparent background for inactive slots
        }
    }

    pub fn with_label<L, C>(slot_num: usize, label: L, color_hex: C) -> Self
    where
        L: Into<String>,
        C: Into<String>,
    {
        Self {
            slot_num,
            label: label.into(),
            color_hex: color_hex.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionState {
    Idle,
    Recording,
    Processing,
    Reviewing,
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
    destination_buttons: Vec<Button>,
    destination_box: Box,
    destinations: Arc<Mutex<Vec<DestinationSlot>>>,

    // Callbacks
    on_manual_stop: Option<Arc<dyn Fn() + Send + Sync>>,
    on_send_text: Option<Arc<dyn Fn(String, usize) + Send + Sync>>,
    on_cancel: Option<Arc<dyn Fn() + Send + Sync>>,
    on_stop_and_send: Option<Arc<dyn Fn(usize) + Send + Sync>>, // Stop recording and send to slot
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
        level_bar.set_size_request(-1, 2); // Full width, 2px height
        level_bar.set_vexpand(false);
        level_bar.set_valign(gtk4::Align::Start);
        main_box.append(&level_bar);

        // Destination slots bar (colored buttons for Kitty mode)
        let destination_box = Box::new(Orientation::Horizontal, 0);
        destination_box.set_visible(false); // Hidden by default, shown in Kitty mode
        destination_box.set_homogeneous(false); // Buttons can be different widths
        let mut destination_buttons = Vec::new();
        for _ in 0..8 {
            let button = Button::new();
            button.set_label(" ");
            button.set_size_request(-1, 24);
            button.set_hexpand(false); // Don't expand horizontally
            destination_box.append(&button);
            destination_buttons.push(button);
        }
        main_box.append(&destination_box);

        // Content area with padding
        let content_box = Box::new(Orientation::Vertical, 10);
        content_box.set_margin_top(15);
        content_box.set_margin_bottom(10);
        content_box.set_margin_start(20);
        content_box.set_margin_end(20);

        // Text preview label (shown during recording/processing)
        let text_preview = Label::new(None);
        text_preview.set_wrap(true);
        text_preview.set_xalign(0.0); // Left align
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
        status_label.set_xalign(0.0); // Left align
        status_box.append(&status_label);

        // Microphone info label (right-aligned in status bar)
        let mic_label = Label::new(None);
        mic_label
            .set_markup("<small><span foreground='#888888'>Detecting source...</span></small>");
        mic_label.set_xalign(1.0); // Right align
        mic_label.set_hexpand(true);
        status_box.append(&mic_label);

        main_box.append(&status_box);

        window.set_child(Some(&main_box));

        Self {
            window,
            state: Arc::new(Mutex::new(TranscriptionState::Idle)),
            spinner,
            status_label,
            mic_label,
            level_bar,
            text_view,
            scrolled,
            text_preview,
            destination_buttons,
            destination_box,
            destinations: Arc::new(Mutex::new(Vec::new())),
            on_manual_stop: None,
            on_send_text: None,
            on_cancel: None,
            on_stop_and_send: None,
        }
    }

    pub fn set_destinations(&self, destinations: &[DestinationSlot]) {
        // Store the destinations for use in click handlers
        *self.destinations.lock() = destinations.to_vec();

        // Check if there are any active (non-empty) slots
        let has_active_slots = destinations.iter().any(|slot| !slot.label.is_empty());

        if !has_active_slots {
            self.destination_box.set_visible(false);
            return;
        }

        self.destination_box.set_visible(true);

        for (i, slot) in destinations.iter().enumerate().take(8) {
            if let Some(button) = self.destination_buttons.get(i) {
                // Only show button if slot has content
                if slot.label.is_empty() {
                    button.set_visible(false);
                    continue;
                }

                let slot_num = i + 1;

                // Create horizontal box: icon on left, text on right
                let hbox = Box::new(Orientation::Horizontal, 6);

                // Icon label (takes up full height)
                let icon_label = Label::new(Some(&slot_unicode_char(slot_num).to_string()));
                icon_label.set_markup(&format!(
                    "<span size='large'>{}</span>",
                    slot_unicode_char(slot_num)
                ));
                icon_label.set_valign(gtk4::Align::Start);
                hbox.append(&icon_label);

                // Parse label for multi-line support
                // Format: "repo_name\nbranch\nmatched_cmd"
                let lines: Vec<&str> = slot.label.split('\n').collect();

                if lines.len() > 1 {
                    // Multi-line label: create a vertical box with styled labels
                    let vbox = Box::new(Orientation::Vertical, 2);

                    // First line (repo name) - regular color
                    let line1 = Label::new(Some(lines[0]));
                    line1.set_markup(&format!("<small>{}</small>", lines[0]));
                    line1.set_xalign(0.0);
                    vbox.append(&line1);

                    // Second line (branch name) - yellow
                    if let Some(line2_text) = lines.get(1) {
                        let line2 = Label::new(Some(line2_text));
                        line2.set_markup(&format!(
                            "<small><span foreground='#d4a017'>{line2_text}</span></small>",
                        ));
                        line2.set_xalign(0.0);
                        vbox.append(&line2);
                    }

                    // Third line (matched command) - cyan italic
                    if let Some(line3_text) = lines.get(2) {
                        let line3 = Label::new(Some(line3_text));
                        line3.set_markup(&format!(
                            "<small><span foreground='#00bcd4' style='italic'>{line3_text}</span></small>",
                        ));
                        line3.set_xalign(0.0);
                        vbox.append(&line3);
                    }

                    hbox.append(&vbox);
                } else {
                    // Single line label
                    let text_label = Label::new(Some(&slot.label));
                    text_label.set_markup(&format!("<small>{}</small>", &slot.label));
                    text_label.set_xalign(0.0);
                    text_label.set_valign(gtk4::Align::Center);
                    hbox.append(&text_label);
                }

                button.set_child(Some(&hbox));

                // Create CSS provider for background color
                let css_provider = gtk4::CssProvider::new();
                let css = format!(
                    "button {{ background-color: {}; padding: 4px 8px; font-size: small; }}",
                    slot.color_hex
                );
                css_provider.load_from_data(&css);

                // Apply CSS to the button
                button
                    .style_context()
                    .add_provider(&css_provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

                button.set_visible(true);
            }
        }

        // Hide unused slots
        for i in destinations.len()..8 {
            if let Some(button) = self.destination_buttons.get(i) {
                button.set_visible(false);
            }
        }
    }

    pub fn setup_destination_click_handlers(&self) {
        // Setup click handlers for destination buttons
        for (i, button) in self.destination_buttons.iter().enumerate() {
            let button_index = i;
            let state = Arc::clone(&self.state);
            let on_send_text = self.on_send_text.clone();
            let on_stop_and_send = self.on_stop_and_send.clone();
            let text_view = self.text_view.clone();
            let destinations = Arc::clone(&self.destinations);

            button.connect_clicked(move |_| {
                // Get the actual slot_num from the stored destinations
                let slot_num = {
                    let dests = destinations.lock();
                    if let Some(dest) = dests.get(button_index) {
                        dest.slot_num
                    } else {
                        tracing::warn!("Button {} clicked but no destination found", button_index);
                        return;
                    }
                };

                tracing::info!(
                    "Destination button {} clicked (slot {})",
                    button_index,
                    slot_num
                );
                let current_state = *state.lock();
                use TranscriptionState::*;
                match current_state {
                    Recording | Processing => {
                        // Stop recording and send to specific slot
                        tracing::info!("Stopping recording and sending to slot {}", slot_num);
                        if let Some(ref callback) = on_stop_and_send {
                            callback(slot_num);
                        }
                    }
                    Reviewing => {
                        // Send text from editable view to specific slot
                        tracing::info!("Sending text from review to slot {}", slot_num);
                        if let Some(ref callback) = on_send_text {
                            let buffer = text_view.buffer();
                            let text = buffer
                                .text(&buffer.start_iter(), &buffer.end_iter(), false)
                                .to_string();
                            callback(text, slot_num);
                        }
                    }
                    _ => {
                        tracing::warn!("Button clicked in state {:?}, ignoring", current_state);
                    }
                }
            });
        }
    }

    pub fn connect_close_handler<F>(&self, on_close: F)
    where
        F: Fn() + 'static,
    {
        let window = self.window.clone();
        window.connect_close_request(move |_| {
            tracing::info!("Window close requested");
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
        F: Fn(String, usize) + Send + Sync + 'static,
    {
        self.on_send_text = Some(Arc::new(callback));
    }

    pub fn set_on_cancel<F>(&mut self, callback: F)
    where
        F: Fn() + Send + Sync + 'static,
    {
        self.on_cancel = Some(Arc::new(callback));
    }

    pub fn set_on_stop_and_send<F>(&mut self, callback: F)
    where
        F: Fn(usize) + Send + Sync + 'static,
    {
        self.on_stop_and_send = Some(Arc::new(callback));
    }

    pub fn setup_key_handlers(&self) {
        let state = Arc::clone(&self.state);
        let on_manual_stop = self.on_manual_stop.clone();
        let on_manual_stop_clone = on_manual_stop.clone();
        let on_send_text = self.on_send_text.clone();
        let on_stop_and_send = self.on_stop_and_send.clone();
        let on_stop_and_send_clone = on_stop_and_send.clone();
        let on_cancel = self.on_cancel.clone();
        let text_view = self.text_view.clone();
        let window = self.window.clone();
        let destinations = Arc::clone(&self.destinations);

        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
            let current_state = *state.lock();

            // Escape key handling
            if keyval == gdk::Key::Escape {
                use TranscriptionState::*;
                match current_state {
                    Recording | Processing => {
                        // Stop recording/transcribing and go to editing view
                        if let Some(ref callback) = on_manual_stop {
                            callback();
                        }
                        return glib::Propagation::Stop;
                    }
                    Reviewing => {
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

            // Alt+Enter key handling - stop and send to all slots during recording, or send text to all during reviewing
            if (keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter)
                && modifiers.contains(gdk::ModifierType::ALT_MASK)
            {
                handle_send_to_all(
                    current_state,
                    &destinations,
                    &text_view,
                    on_stop_and_send.as_ref(),
                    on_send_text.as_ref(),
                );
                return glib::Propagation::Stop;
            }

            // Alt+1 through Alt+8 key handling - stop and send to specific destination
            if modifiers.contains(gdk::ModifierType::ALT_MASK) {
                // Map key number to button position (0-based index)
                let button_index = match keyval {
                    gdk::Key::_1 | gdk::Key::KP_1 => Some(0),
                    gdk::Key::_2 | gdk::Key::KP_2 => Some(1),
                    gdk::Key::_3 | gdk::Key::KP_3 => Some(2),
                    gdk::Key::_4 | gdk::Key::KP_4 => Some(3),
                    gdk::Key::_5 | gdk::Key::KP_5 => Some(4),
                    gdk::Key::_6 | gdk::Key::KP_6 => Some(5),
                    gdk::Key::_7 | gdk::Key::KP_7 => Some(6),
                    gdk::Key::_8 | gdk::Key::KP_8 => Some(7),
                    _ => None,
                };

                if let Some(idx) = button_index {
                    // Look up the actual slot_num from the destinations
                    let slot_num = {
                        let dests = destinations.lock();
                        if let Some(dest) = dests.get(idx) {
                            Some(dest.slot_num)
                        } else {
                            tracing::warn!(
                                "Alt+{} pressed but no destination at position {}",
                                idx + 1,
                                idx
                            );
                            None
                        }
                    };

                    if let Some(dest) = slot_num {
                        handle_send_to_slot(
                            current_state,
                            dest,
                            &text_view,
                            on_stop_and_send.as_ref(),
                            on_send_text.as_ref(),
                        );
                        return glib::Propagation::Stop;
                    }
                }
            }

            glib::Propagation::Proceed
        });

        self.window.add_controller(key_controller);

        // Also add key controller to text view
        let state = Arc::clone(&self.state);
        let on_send_text = self.on_send_text.clone();
        let on_cancel = self.on_cancel.clone();
        let text_view_clone = self.text_view.clone();
        let destinations_clone = Arc::clone(&self.destinations);

        let text_key_controller = gtk4::EventControllerKey::new();
        text_key_controller.connect_key_pressed(move |_controller, keyval, _keycode, modifiers| {
            let current_state = *state.lock();

            // Escape key handling in text view
            if keyval == gdk::Key::Escape {
                use TranscriptionState::*;
                match current_state {
                    Recording | Processing => {
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

            // Alt+Enter in text view - stop and send during recording, or send during reviewing
            if (keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter)
                && modifiers.contains(gdk::ModifierType::ALT_MASK)
            {
                handle_send_to_all(
                    current_state,
                    &destinations_clone,
                    &text_view_clone,
                    on_stop_and_send_clone.as_ref(),
                    on_send_text.as_ref(),
                );
                return glib::Propagation::Stop;
            }

            // Alt+1 through Alt+8 in text view
            if modifiers.contains(gdk::ModifierType::ALT_MASK) {
                // Map key number to button position (0-based index)
                let button_index = match keyval {
                    gdk::Key::_1 | gdk::Key::KP_1 => Some(0),
                    gdk::Key::_2 | gdk::Key::KP_2 => Some(1),
                    gdk::Key::_3 | gdk::Key::KP_3 => Some(2),
                    gdk::Key::_4 | gdk::Key::KP_4 => Some(3),
                    gdk::Key::_5 | gdk::Key::KP_5 => Some(4),
                    gdk::Key::_6 | gdk::Key::KP_6 => Some(5),
                    gdk::Key::_7 | gdk::Key::KP_7 => Some(6),
                    gdk::Key::_8 | gdk::Key::KP_8 => Some(7),
                    _ => None,
                };

                if let Some(idx) = button_index {
                    // Look up the actual slot_num from the destinations
                    let slot_num = {
                        let dests = destinations_clone.lock();
                        if let Some(dest) = dests.get(idx) {
                            Some(dest.slot_num)
                        } else {
                            tracing::warn!(
                                "Alt+{} pressed in text view but no destination at position {}",
                                idx + 1,
                                idx
                            );
                            None
                        }
                    };

                    if let Some(dest) = slot_num {
                        handle_send_to_slot(
                            current_state,
                            dest,
                            &text_view_clone,
                            on_stop_and_send_clone.as_ref(),
                            on_send_text.as_ref(),
                        );
                        return glib::Propagation::Stop;
                    }
                }

                // Readline-style word navigation (Alt+B, Alt+F)
                match keyval {
                    gdk::Key::b | gdk::Key::B => {
                        // Alt+B: backward word
                        text_view_clone.emit_move_cursor(MovementStep::Words, -1, false);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::f | gdk::Key::F => {
                        // Alt+F: forward word
                        text_view_clone.emit_move_cursor(MovementStep::Words, 1, false);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::d | gdk::Key::D => {
                        // Alt+D: delete word forward
                        text_view_clone.emit_delete_from_cursor(DeleteType::WordEnds, 1);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::BackSpace => {
                        // Alt+Backspace: delete word backward
                        text_view_clone.emit_delete_from_cursor(DeleteType::WordEnds, -1);
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }

            // Readline-style Ctrl bindings
            if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                match keyval {
                    gdk::Key::a | gdk::Key::A => {
                        // Ctrl+A: beginning of line
                        text_view_clone.emit_move_cursor(MovementStep::DisplayLineEnds, -1, false);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::e | gdk::Key::E => {
                        // Ctrl+E: end of line
                        text_view_clone.emit_move_cursor(MovementStep::DisplayLineEnds, 1, false);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::k | gdk::Key::K => {
                        // Ctrl+K: kill to end of line
                        text_view_clone.emit_delete_from_cursor(DeleteType::DisplayLineEnds, 1);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::u | gdk::Key::U => {
                        // Ctrl+U: kill to beginning of line
                        text_view_clone.emit_delete_from_cursor(DeleteType::DisplayLineEnds, -1);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::w | gdk::Key::W => {
                        // Ctrl+W: delete word backward
                        text_view_clone.emit_delete_from_cursor(DeleteType::WordEnds, -1);
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }

            glib::Propagation::Proceed
        });

        self.text_view.add_controller(text_key_controller);
    }

    pub fn update_state(&self, state: TranscriptionState, message: &str, level: f64) {
        *self.state.lock() = state;

        use TranscriptionState::*;
        match state {
            Recording => {
                self.spinner.start();
                self.status_label
                    .set_markup(&format!("<small>{}</small>", message));
                self.level_bar.set_visible(true);
                self.level_bar.set_value(level.min(0.1));
                tracing::trace!("Level bar updated: {}", level);
                self.text_preview.set_visible(true);
                self.scrolled.set_visible(false);
            }
            Processing => {
                self.spinner.start();
                self.status_label
                    .set_markup(&format!("<small>⚙️ {}</small>", message));
                self.level_bar.set_visible(true);
                self.level_bar.set_value(level.min(0.1));
                self.text_preview.set_visible(true); // Show text preview during processing
                self.scrolled.set_visible(false); // Hide editable text view during processing
            }
            Reviewing => {
                self.spinner.stop();
                self.status_label.set_markup(&format!(
                    "<small>✓ {} • <span foreground='#888888'>Alt+Enter to send • Escape to cancel</span></small>",
                    message
                ));
                self.level_bar.set_visible(false);
                self.text_preview.set_visible(false);
                self.scrolled.set_visible(true);
                self.text_view.set_editable(true); // Editable during reviewing
            }
            Error => {
                self.spinner.stop();
                self.status_label
                    .set_markup(&format!("<small>❌ {}</small>", message));
                self.level_bar.set_visible(false);
            }
            Idle => {
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
        // Show preview text in gray to indicate it may be corrected
        let escaped = glib::markup_escape_text(text);
        self.text_preview
            .set_markup(&format!("<span foreground='#888888'>{escaped}</span>"));
    }

    pub fn set_confirmed_and_preview(&self, confirmed: &str, preview: &str) {
        // Show confirmed text in white, preview text in gray
        let confirmed_escaped = glib::markup_escape_text(confirmed);
        let preview_escaped = glib::markup_escape_text(preview);

        let markup = if confirmed.is_empty() {
            format!("<span foreground='#888888'>{preview_escaped}</span>")
        } else if preview.is_empty() {
            confirmed_escaped.to_string()
        } else {
            format!("{confirmed_escaped} <span foreground='#888888'>{preview_escaped}</span>")
        };

        self.text_preview.set_markup(&markup);
    }

    pub fn set_transcribed_text(&self, text: &str) {
        let buffer = self.text_view.buffer();
        buffer.set_text(text);
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    pub fn reset(&self) {
        tracing::info!("Resetting dialog state");

        // Reset transcription state
        *self.state.lock() = TranscriptionState::Idle;
        *self.destinations.lock() = Vec::new();

        // Clear text content
        self.text_view.buffer().set_text("");
        self.text_preview.set_text("");

        // Reset UI elements to initial state
        self.spinner.stop();
        self.status_label
            .set_markup("<small>Waiting for speech...</small>");
        self.level_bar.set_value(0.0);
        self.level_bar.set_visible(true);
        self.text_preview.set_visible(false);
        self.scrolled.set_visible(false);
        self.destination_box.set_visible(false);

        // Reset mic label
        self.mic_label.set_text("");
    }

    pub fn close(&self) {
        self.window.close();
    }

    #[allow(dead_code)]
    pub fn window(&self) -> &ApplicationWindow {
        &self.window
    }
}
