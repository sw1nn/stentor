use anyhow::{Context, Result};
use libpulse_binding::context::{Context as PulseContext, FlagSet as ContextFlagSet};
use libpulse_binding::mainloop::standard::{IterateResult, Mainloop};
use std::cell::RefCell;
use std::rc::Rc;

/// Manages source mute state using libpulse (PulseAudio/PipeWire)
pub struct SourceMuteManager {
    was_muted: bool,
    source_index: Option<u32>,
}

impl SourceMuteManager {
    /// Check if source is muted and unmute it if requested
    /// Returns a manager that will restore the original state on drop
    pub fn unmute_if_needed() -> Result<Self> {
        let (source_index, was_muted) = Self::get_default_source_info()?;

        if was_muted {
            tracing::info!("Source was muted, unmuting for recording");
            Self::set_mute(source_index, false)?;
        } else {
            tracing::debug!("Source was already unmuted");
        }

        Ok(Self {
            was_muted,
            source_index: Some(source_index),
        })
    }

    /// Get the default audio source index and mute state
    fn get_default_source_info() -> Result<(u32, bool)> {
        let mut mainloop = Mainloop::new().context("Failed to create PulseAudio mainloop")?;
        let mut context = PulseContext::new(&mainloop, "stentor-source-query")
            .context("Failed to create PulseAudio context")?;

        context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .context("Failed to connect to PulseAudio")?;

        // Wait for context to be ready
        loop {
            match mainloop.iterate(false) {
                IterateResult::Quit(_) | IterateResult::Err(_) => {
                    anyhow::bail!("PulseAudio mainloop failed");
                }
                IterateResult::Success(_) => {}
            }

            match context.get_state() {
                libpulse_binding::context::State::Ready => {
                    tracing::debug!("PulseAudio context ready");
                    break;
                }
                libpulse_binding::context::State::Failed | libpulse_binding::context::State::Terminated => {
                    anyhow::bail!("PulseAudio context failed");
                }
                _ => {}
            }
        }

        let default_source_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let name_clone = Rc::clone(&default_source_name);

        // Get server info to find default source name
        let introspect = context.introspect();
        introspect.get_server_info(move |server_info| {
            if let Some(name) = server_info.default_source_name.as_ref() {
                tracing::debug!("Default source name from server: {}", name);
                *name_clone.borrow_mut() = Some(name.to_string());
            } else {
                tracing::warn!("No default source name in server info");
            }
        });

        // Process events until we have server info (use blocking iterations)
        for i in 0..50 {
            match mainloop.iterate(true) {  // true = blocking
                IterateResult::Quit(_) | IterateResult::Err(_) => {
                    tracing::debug!("Mainloop quit or error at iteration {}", i);
                    break;
                }
                IterateResult::Success(_) => {}
            }

            if default_source_name.borrow().is_some() {
                tracing::debug!("Got default source name after {} iterations", i);
                break;
            }
        }

        let source_name = default_source_name.borrow().clone()
            .ok_or_else(|| anyhow::anyhow!("Failed to get default source name from PulseAudio"))?;

        tracing::debug!("Looking for source: {}", source_name);

        // Now get the actual source info by name
        let result: Rc<RefCell<Option<(u32, bool)>>> = Rc::new(RefCell::new(None));
        let result_clone = Rc::clone(&result);

        let introspect = context.introspect();
        introspect.get_source_info_by_name(&source_name, move |list_result| {
            match list_result {
                libpulse_binding::callbacks::ListResult::Item(source_info) => {
                    let index = source_info.index;
                    let is_muted = source_info.mute;
                    let name = source_info.name.as_ref().map(|s| s.to_string()).unwrap_or_else(|| "unknown".to_string());
                    tracing::debug!("Found source '{}' with index: {}, muted: {}", name, index, is_muted);
                    *result_clone.borrow_mut() = Some((index, is_muted));
                }
                libpulse_binding::callbacks::ListResult::End => {
                    tracing::debug!("Source info list end");
                }
                libpulse_binding::callbacks::ListResult::Error => {
                    tracing::error!("Error getting source info");
                }
            }
        });

        // Process events until we have the result (use blocking iterations)
        for i in 0..50 {
            match mainloop.iterate(true) {  // true = blocking
                IterateResult::Quit(_) | IterateResult::Err(_) => {
                    tracing::debug!("Mainloop quit or error at iteration {}", i);
                    break;
                }
                IterateResult::Success(_) => {}
            }

            if result.borrow().is_some() {
                tracing::debug!("Got source info after {} iterations", i);
                break;
            }
        }

        let source_info = *result.borrow();
        source_info.ok_or_else(|| anyhow::anyhow!("Failed to get source info for '{}'", source_name))
    }

    /// Set mute state for the source
    fn set_mute(source_index: u32, mute: bool) -> Result<()> {
        let mut mainloop = Mainloop::new().context("Failed to create PulseAudio mainloop")?;
        let mut context = PulseContext::new(&mainloop, "stentor-source-mute")
            .context("Failed to create PulseAudio context")?;

        context
            .connect(None, ContextFlagSet::NOFLAGS, None)
            .context("Failed to connect to PulseAudio")?;

        // Wait for context to be ready
        loop {
            match mainloop.iterate(false) {
                IterateResult::Quit(_) | IterateResult::Err(_) => {
                    anyhow::bail!("PulseAudio mainloop failed");
                }
                IterateResult::Success(_) => {}
            }

            match context.get_state() {
                libpulse_binding::context::State::Ready => break,
                libpulse_binding::context::State::Failed | libpulse_binding::context::State::Terminated => {
                    anyhow::bail!("PulseAudio context failed");
                }
                _ => {}
            }
        }

        let done = Rc::new(RefCell::new(false));
        let done_clone = Rc::clone(&done);

        let mut introspector = context.introspect();
        introspector.set_source_mute_by_index(source_index, mute, Some(Box::new(move |success| {
            *done_clone.borrow_mut() = true;
            if !success {
                tracing::error!("Failed to set mute state");
            }
        })));

        // Process events until operation completes (use blocking iterations)
        for _ in 0..50 {
            match mainloop.iterate(true) {  // true = blocking
                IterateResult::Quit(_) | IterateResult::Err(_) => break,
                IterateResult::Success(_) => {}
            }

            if *done.borrow() {
                break;
            }
        }

        tracing::debug!("Set source {} to {}", source_index, if mute { "muted" } else { "unmuted" });
        Ok(())
    }
}

impl Drop for SourceMuteManager {
    fn drop(&mut self) {
        // Restore original mute state
        if let Some(source_index) = self.source_index {
            if self.was_muted {
                tracing::info!("Restoring source to muted state");
                if let Err(e) = Self::set_mute(source_index, true) {
                    tracing::error!("Failed to restore mute state: {}", e);
                }
            } else {
                tracing::debug!("Source was not muted originally, leaving unmuted");
            }
        }
    }
}
