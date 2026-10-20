//! Push-to-talk key handling.
//!
//! A momentary foot pedal sends a press and a release. A hold is push-to-talk:
//! the session runs while the key is down and stops on release. A tap shorter
//! than the tap threshold latches the session open until the next press.
//!
//! The state lives in the daemon so a daemon restart cannot leave a stale latch
//! behind, and so the press timing does not depend on the client.

use std::time::{Duration, Instant};

/// The kind of recording session that is currently running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    HandsFree,
    Ptt,
}

/// What the daemon should do after a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Leave the session as it is.
    Ignore,
    /// Start a new push-to-talk session.
    Start,
    /// Stop the active session and send the transcription.
    StopAndSend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Pressed(Instant),
    Latched,
    Stopping,
}

/// Push-to-talk press and release state.
pub struct PttState {
    phase: Phase,
    tap_threshold: Duration,
}

impl PttState {
    pub fn new(tap_threshold: Duration) -> Self {
        Self {
            phase: Phase::Idle,
            tap_threshold,
        }
    }

    /// Handle `ptt-down`. `active` is the session that is running, if any.
    #[must_use]
    pub fn press(&mut self, active: Option<SessionKind>) -> Action {
        match active {
            None => {
                self.phase = Phase::Pressed(Instant::now());
                Action::Start
            }
            Some(SessionKind::Ptt) => match self.phase {
                Phase::Latched => {
                    self.phase = Phase::Stopping;
                    Action::StopAndSend
                }
                Phase::Pressed(_) | Phase::Stopping | Phase::Idle => {
                    // A duplicate press, or a press while the previous session
                    // is still finishing. Drop it rather than arm the phase.
                    Action::Ignore
                }
            },
            // A pedal press must not change a hands-free session.
            Some(SessionKind::HandsFree) => Action::Ignore,
        }
    }

    /// Handle `ptt-up`. `active` is the session that is running, if any.
    #[must_use]
    pub fn release(&mut self, active: Option<SessionKind>) -> Action {
        match (active, self.phase) {
            (Some(SessionKind::Ptt), Phase::Pressed(started))
                if started.elapsed() < self.tap_threshold =>
            {
                self.phase = Phase::Latched;
                Action::Ignore
            }
            (Some(SessionKind::Ptt), Phase::Pressed(_)) => {
                self.phase = Phase::Idle;
                Action::StopAndSend
            }
            (Some(SessionKind::Ptt), Phase::Stopping) => {
                self.phase = Phase::Idle;
                Action::Ignore
            }
            (Some(SessionKind::Ptt), Phase::Idle | Phase::Latched) => Action::Ignore,
            (None | Some(SessionKind::HandsFree), _) => {
                self.phase = Phase::Idle;
                Action::Ignore
            }
        }
    }

    /// Forget the current phase. Call this when no session is active, so a
    /// session that ended on its own cannot leave a stale latch.
    pub fn reset(&mut self) {
        self.phase = Phase::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> PttState {
        PttState::new(Duration::from_millis(300))
    }

    #[test]
    fn hold_starts_then_stops_and_sends() {
        let mut ptt = state();
        assert_eq!(ptt.press(None), Action::Start);

        ptt.phase = Phase::Pressed(Instant::now() - Duration::from_millis(400));
        assert_eq!(ptt.release(Some(SessionKind::Ptt)), Action::StopAndSend);
        assert_eq!(ptt.phase, Phase::Idle);
    }

    #[test]
    fn tap_latches_the_session_open() {
        let mut ptt = state();
        assert_eq!(ptt.press(None), Action::Start);
        assert_eq!(ptt.release(Some(SessionKind::Ptt)), Action::Ignore);
        assert_eq!(ptt.phase, Phase::Latched);

        assert_eq!(ptt.press(Some(SessionKind::Ptt)), Action::StopAndSend);
        assert_eq!(ptt.phase, Phase::Stopping);
        assert_eq!(ptt.release(Some(SessionKind::Ptt)), Action::Ignore);
        assert_eq!(ptt.phase, Phase::Idle);
    }

    #[test]
    fn release_while_latched_is_ignored() {
        let mut ptt = state();
        let _ = ptt.press(None);
        let _ = ptt.release(Some(SessionKind::Ptt));
        assert_eq!(ptt.release(Some(SessionKind::Ptt)), Action::Ignore);
        assert_eq!(ptt.phase, Phase::Latched);
    }

    #[test]
    fn duplicate_press_is_ignored() {
        let mut ptt = state();
        assert_eq!(ptt.press(None), Action::Start);
        assert_eq!(ptt.press(Some(SessionKind::Ptt)), Action::Ignore);
    }

    #[test]
    fn press_during_hands_free_session_is_ignored() {
        let mut ptt = state();
        assert_eq!(ptt.press(Some(SessionKind::HandsFree)), Action::Ignore);
        assert_eq!(ptt.phase, Phase::Idle);
    }

    #[test]
    fn release_during_hands_free_session_is_ignored() {
        let mut ptt = state();
        ptt.phase = Phase::Pressed(Instant::now());
        assert_eq!(ptt.release(Some(SessionKind::HandsFree)), Action::Ignore);
        assert_eq!(ptt.phase, Phase::Idle);
    }

    #[test]
    fn press_while_finalising_is_ignored() {
        // The session is still PTT but the phase is already idle, so a press
        // must not arm a new one.
        let mut ptt = state();
        assert_eq!(ptt.press(Some(SessionKind::Ptt)), Action::Ignore);
        assert_eq!(ptt.phase, Phase::Idle);
    }

    #[test]
    fn release_with_no_session_resets() {
        let mut ptt = state();
        ptt.phase = Phase::Latched;
        assert_eq!(ptt.release(None), Action::Ignore);
        assert_eq!(ptt.phase, Phase::Idle);
    }

    #[test]
    fn zero_threshold_disables_latching() {
        let mut ptt = PttState::new(Duration::ZERO);
        assert_eq!(ptt.press(None), Action::Start);
        assert_eq!(ptt.release(Some(SessionKind::Ptt)), Action::StopAndSend);
    }

    #[test]
    fn reset_returns_to_idle() {
        let mut ptt = state();
        let _ = ptt.press(None);
        let _ = ptt.release(Some(SessionKind::Ptt));
        ptt.reset();
        assert_eq!(ptt.phase, Phase::Idle);
        assert_eq!(ptt.release(None), Action::Ignore);
    }
}
