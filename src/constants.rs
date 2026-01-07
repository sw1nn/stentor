//! Application-wide constants
//!
//! This module centralizes magic numbers to make them easier to find,
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
