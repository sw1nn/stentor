# Rust Implementation Status

## ✅ Completed

### Core Infrastructure
- [x] **Configuration** - TOML config with XDG support, CLI override
- [x] **Transcription** - Whisper integration with automatic model download
- [x] **Daemon/Client** - Unix socket communication architecture
- [x] **Dialog UI** - GTK4/libadwaita UI with keyboard handlers
- [x] **Audio Recording** - cpal audio capture with VAD
- [x] **CLI** - Complete CLI for both daemon and client binaries
- [x] **Progress Bar** - Download progress with indicatif

### Features Working
- ✅ Daemon starts and loads Whisper model on startup
- ✅ Model downloads automatically with progress bar if not present
- ✅ Client can connect to daemon via Unix socket
- ✅ Dialog window opens on "start" command
- ✅ Configuration loads from `${XDG_CONFIG_HOME}/stentor/config.toml`
- ✅ CLI arguments override config file settings

## 🚧 To Be Implemented

### 1. Recording Workflow
**File**: `src/main.rs` (DaemonCommand::Start handler)

Need to:
- Create AudioRecorder when dialog opens
- Start audio stream in background thread
- Feed audio chunks through VAD
- Accumulate recorded audio until stop condition

**Code Sketch**:
```rust
// In DaemonCommand::Start
let recorder = AudioRecorder::new(16000, config.silence_threshold)?;
let (chunk_tx, chunk_rx) = mpsc::channel();
let (cmd_tx, cmd_rx) = mpsc::channel();

// Start recording
let stream = recorder.start_recording(chunk_tx, Arc::new(Mutex::new(cmd_rx)))?;

// Process chunks in background
tokio::spawn(async move {
    let mut vad = VoiceActivityDetector::new(...);
    let mut recorded_audio = Vec::new();

    while let Ok(chunk) = chunk_rx.recv() {
        recorded_audio.push(chunk.data);
        let vad_result = vad.process_chunk(...);

        if vad_result.should_stop {
            // Trigger transcription
            break;
        }
    }
});
```

### 2. Dialog Callbacks
**File**: `src/main.rs` + `src/dialog.rs`

Need to wire up:
- `set_on_manual_stop` - User presses Escape during recording
- `set_on_send_text` - User presses Ctrl+Enter in review
- `set_on_cancel` - User presses Escape in review

**Code Sketch**:
```rust
let (stop_tx, stop_rx) = mpsc::channel();
dialog.set_on_manual_stop(move || {
    stop_tx.send(()).unwrap();
});

let (send_tx, send_rx) = mpsc::channel();
dialog.set_on_send_text(move |text| {
    send_tx.send(text).unwrap();
});
```

### 3. Transcription Trigger
**File**: `src/main.rs`

When recording stops (VAD or manual):
```rust
// Concatenate all audio chunks
let audio_flat: Vec<f32> = recorded_audio.into_iter().flatten().collect();

// Transcribe in blocking thread
let transcriber_clone = Arc::clone(&transcriber);
let result = tokio::task::spawn_blocking(move || {
    transcriber_clone.transcribe(&audio_flat)
}).await??;

// Show in dialog for review
dialog.set_transcribed_text(&result);
dialog.update_state(TranscriptionState::Reviewing, "Ready to send", 0.0);
```

### 4. Text Output
**File**: `src/main.rs`

When user confirms (Ctrl+Enter):
```rust
if let Some(ref cmd) = config.output_command {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("TRANSCRIPTION", text)
        .output()?;
}
dialog.close();
```

### 5. State Management
Need to track:
- Current recording state (idle/recording/processing/reviewing)
- Audio stream handle (to stop it)
- Transcriber instance (shared across requests)
- Currently active dialog

Suggest creating a `RecordingSession` struct:
```rust
struct RecordingSession {
    stream: Option<Stream>,
    recorded_audio: Vec<Vec<f32>>,
    vad: VoiceActivityDetector,
    state: VadState,
    // ...
}
```

## 📋 Integration Steps

1. **Add shared state**: Wrap Transcriber in `Arc<Transcriber>` for sharing
2. **Implement recording loop**: Background thread with VAD processing
3. **Wire dialog callbacks**: Connect UI events to recording control
4. **Add transcription**: Call whisper when recording stops
5. **Implement output**: Execute output_command with text
6. **Handle cleanup**: Properly stop streams and close dialogs

## 🎯 Quick Win: Minimal Working Version

For a basic working version, you could:

1. Skip VAD for now - just record for fixed 3 seconds
2. Hardcode a test phrase or use silence
3. Call transcriber directly
4. Print result to console (skip output_command)

This would prove the integration works before adding complexity.

## 📝 Notes

- Recording loop runs in background thread
- GTK UI updates via `glib::idle_add()` for thread safety
- Need careful thread synchronization between audio, GTK, and tokio
