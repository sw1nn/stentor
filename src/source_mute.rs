use anyhow::{Context, Result};
use std::sync::Arc;

use crate::pulse::PulseManager;

/// Manages source mute state via the supervised PulseAudio connection.
///
/// The source is addressed by name, never by index: indexes are not stable
/// across a PulseAudio / PipeWire restart.
pub struct SourceMuteManager {
    was_muted: bool,
    source_name: String,
    pulse: Arc<PulseManager>,
}

impl SourceMuteManager {
    /// Unmute the default source if it is muted.
    ///
    /// Returns a manager that restores the original mute state on drop.
    pub fn unmute_if_needed(pulse: Arc<PulseManager>) -> Result<Self> {
        let source_name = pulse
            .default_source_name()
            .context("could not determine the default audio source")?;

        let was_muted = pulse.source_mute(&source_name).unwrap_or(false);

        if was_muted {
            tracing::info!(source = %source_name, "Source was muted, unmuting for recording");
            if !pulse.set_mute(&source_name, false) {
                tracing::warn!(source = %source_name, "Failed to unmute source");
            }
        } else {
            tracing::debug!(source = %source_name, "Source was already unmuted");
        }

        Ok(Self {
            was_muted,
            source_name,
            pulse,
        })
    }
}

impl Drop for SourceMuteManager {
    fn drop(&mut self) {
        if self.was_muted {
            tracing::info!(source = %self.source_name, "Restoring source to muted state");
            if !self.pulse.set_mute(&self.source_name, true) {
                tracing::warn!(source = %self.source_name, "Failed to restore mute state");
            }
        }
    }
}
