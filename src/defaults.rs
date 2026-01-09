//! Application-wide default values
//!
//! These values could potentially be made configurable in the future, but for
//! now they are hardcoded here. Centralizing them makes them easier to find,
//! document, and tune.

use std::time::Duration;

// =============================================================================
// Channel Capacities
// =============================================================================

/// Capacity for the daemon command channel (stentorctl → stentord).
/// Small capacity is fine since commands are infrequent.
pub const COMMAND_CHANNEL_CAPACITY: usize = 32;

/// Capacity for UI message channel (recording thread → GTK).
/// Power of 2 for efficient modulo operations.
/// 128 messages ≈ 8 seconds of buffering at typical message rate.
pub const UI_MESSAGE_CHANNEL_CAPACITY: usize = 128;

/// Capacity for multi-slot handler message channel.
pub const HANDLER_CHANNEL_CAPACITY: usize = 32;

// =============================================================================
// Timeouts and Intervals
// =============================================================================

/// Timeout for receiving audio chunks from the recording thread.
/// Short timeout allows checking for stop commands frequently.
pub const CHUNK_RECV_TIMEOUT: Duration = Duration::from_millis(100);

/// Sleep interval when polling PulseAudio for context/introspection readiness.
/// 10ms × 100 iterations = 1 second max wait.
pub const PULSE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Sleep interval when waiting for PulseAudio introspection results.
pub const PULSE_INTROSPECT_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum iterations when waiting for PulseAudio context to become ready.
/// Combined with PULSE_POLL_INTERVAL gives 1 second timeout.
pub const PULSE_READY_MAX_ITERATIONS: u32 = 100;

/// Timeout for Kitty IPC socket operations.
pub const KITTY_SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

/// Delay before closing dialog after showing error message.
pub const ERROR_DISPLAY_DURATION: Duration = Duration::from_secs(2);

// =============================================================================
// Slot Images
// =============================================================================

/// Image width for slot number overlays (wide aspect ratio to match terminal windows).
pub const SLOT_IMAGE_WIDTH: u32 = 1920;

/// Image height for slot number overlays.
pub const SLOT_IMAGE_HEIGHT: u32 = 1080;

/// Font scale for the slot number digit.
pub const SLOT_IMAGE_FONT_SCALE: f32 = 400.0;

/// Padding from edge for the slot number (in pixels).
pub const SLOT_IMAGE_CORNER_PADDING: f32 = 20.0;

// =============================================================================
// Configuration Defaults
// =============================================================================

/// Default Whisper model size.
pub const MODEL: &str = "base";

/// Default language code for transcription.
pub const LANGUAGE: &str = "en";

/// Default seconds of silence before stopping recording.
pub const SILENCE_DURATION: f32 = 2.0;

/// Default RMS threshold for silence detection.
pub const SILENCE_THRESHOLD: f32 = 0.002;

/// Default minimum speech duration before recording starts.
pub const MIN_SPEECH_DURATION: f32 = 0.5;

/// Default audio chunk size in samples.
pub const CHUNK_SIZE: usize = 1024;

/// Default interval for periodic transcription while speaking (seconds).
pub const PERIODIC_TRANSCRIPTION_INTERVAL: f32 = 1.0;

/// Default sliding window size for transcription (seconds).
pub const TRANSCRIPTION_WINDOW: f32 = 5.0;

/// Default delay before audio is confirmed (seconds).
pub const TRANSCRIPTION_LAG: f32 = 1.0;

/// Default Unix socket filename.
pub const SOCKET_NAME: &str = "stentor.sock";

/// Default Kitty base background color.
pub const KITTY_BASE_BACKGROUND: &str = "#1e1e2e";

/// Default font pattern for slot ID images.
pub const SLOT_ID_FONT: &str = "monospace:bold";
