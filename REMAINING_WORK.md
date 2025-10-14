# Remaining Work to Complete Rust Implementation

## Issue Encountered

GTK widgets (`TranscriptionDialog`) are not `Send`, which means they can't be moved between threads safely. The current implementation tries to pass the dialog to a background thread for recording, which violates Rust's thread safety guarantees.

## Solution Approaches

### Approach 1: Use message passing (Recommended)
Instead of moving the dialog, send messages from the recording thread to update the UI:

```rust
// In main loop
let (ui_tx, ui_rx) = glib::MainContext::channel(glib::Priority::DEFAULT);

// Spawn recording thread
thread::spawn(move || {
    // Send UI updates via channel
    ui_tx.send(UIMessage::UpdateState(...)).unwrap();
    ui_tx.send(UIMessage::SetText(...)).unwrap();
});

// In GTK main context
ui_rx.attach(None, move |msg| {
    match msg {
        UIMessage::UpdateState(state, msg, level) => {
            dialog.update_state(state, msg, level);
        }
        UIMessage::SetText(text) => {
            dialog.set_transcribed_text(&text);
        }
        // ... other messages
    }
    glib::ControlFlow::Continue
});
```

### Approach 2: Keep everything in GTK main thread
Use async/await within the GTK main context instead of spawning threads:

```rust
glib::MainContext::default().spawn_local(async move {
    // Start recording
    let recorder = AudioRecorder::new(...)?;
    // ... recording logic using async channels
});
```

### Approach 3: Only send widget-specific data
Don't move the dialog at all - only send the necessary IDs/handles:

```rust
struct DialogHandle {
    window_id: /* some identifier */,
}

impl DialogHandle {
    fn update_ui(&self, message: String) {
        glib::idle_add_once(move || {
            // Find window by ID and update
        });
    }
}
```

## Recommended Implementation

Use **Approach 1** - it's the cleanest and provides proper thread safety:

### Step 1: Define UI messages
```rust
enum UIMessage {
    UpdateState(TranscriptionState, String, f64),
    SetMicrophone(String),
    SetText(String),
    Close,
}
```

### Step 2: Create channel in main
```rust
let (ui_tx, ui_rx) = glib::MainContext::channel(glib::Priority::DEFAULT);

// Attach receiver
let dialog_clone = dialog.clone();
ui_rx.attach(None, move |msg| {
    match msg {
        UIMessage::UpdateState(state, text, level) => {
            dialog_clone.update_state(state, &text, level);
        }
        UIMessage::SetText(text) => {
            dialog_clone.set_transcribed_text(&text);
        }
        UIMessage::Close => {
            dialog_clone.close();
        }
        // ...
    }
    glib::ControlFlow::Continue
});
```

### Step 3: Pass sender to recording thread
```rust
std::thread::spawn(move || {
    // Recording loop
    for chunk in audio_chunks {
        ui_tx.send(UIMessage::UpdateState(
            TranscriptionState::Recording,
            "Listening...".to_string(),
            chunk.rms as f64
        )).unwrap();
    }

    // After transcription
    ui_tx.send(UIMessage::SetText(result)).unwrap();
    ui_tx.send(UIMessage::UpdateState(
        TranscriptionState::Reviewing,
        "Ready to send".to_string(),
        0.0
    )).unwrap();
});
```

## Files to Modify

1. **src/main.rs** - Add UIMessage enum and channel setup
2. **src/main.rs** - Modify `start_recording_session` to use channel instead of moving dialog
3. Test and verify

## Estimated Time

- 30-60 minutes to implement properly
- The core logic is all there, just needs the threading model fixed

## Alternative: Simpler Async Approach

If you want to avoid thread complexity entirely, you could keep everything async in the GTK main context using `glib::MainContext::spawn_local`. This would be simpler but might have performance implications for CPU-intensive operations like transcription.

## Current Status

✅ All modules implemented and working individually:
- Configuration ✅
- Audio recording ✅
- Voice Activity Detection ✅
- Whisper transcription ✅
- Dialog UI ✅
- Daemon/client ✅

❌ Integration blocked by thread safety issue

The fix is straightforward - it's just a matter of restructuring how we communicate between threads.
