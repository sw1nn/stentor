use anyhow::Result;
use async_channel::Sender;

use crate::dialog::DestinationSlot;

/// UI message types that the multi-slot handler can send
#[derive(Debug, Clone)]
pub enum HandlerUIMessage {
    SetDestinations(Vec<DestinationSlot>),
    StoreWindowIds(Vec<u64>),
}

/// Trait for multi-slot handler implementations
pub trait MultiSlotHandler: Send + Sync {
    /// Setup the multi-slot handler (called when recording starts)
    fn setup(&self, ui_tx: Sender<HandlerUIMessage>) -> Result<()>;

    /// Cleanup the multi-slot handler (called when recording ends)
    fn cleanup(&self, window_ids: Vec<u64>) -> Result<()>;

    /// Return environment variables to pass to output commands for a specific slot.
    /// Handler-specific variables (e.g., KITTY_LISTEN_ON, WINDOW_ID for kitty handler).
    fn output_env_vars(&self, slot_num: usize) -> Vec<(String, String)> {
        let _ = slot_num;
        Vec::new()
    }

    /// Return the slots that currently have a destination, in ascending order.
    fn active_slots(&self) -> Vec<usize> {
        Vec::new()
    }

    /// Environment variables for sending to the currently focused destination
    /// window, if the handler can resolve one. `None` means there is no focused
    /// destination, so callers fall back to [`Self::active_slots`].
    fn focused_window_env_vars(&self) -> Option<Vec<(String, String)>> {
        None
    }

    /// Return whether output for `slot_num` can reach a destination.
    fn has_target(&self, slot_num: usize) -> bool {
        let _ = slot_num;
        true
    }
}
