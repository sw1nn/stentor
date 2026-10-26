use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use thiserror::Error;
use tokio::sync::mpsc;

// The zbus interface and proxy macros require string literals, so these
// constants must stay in sync with the attributes in this module and in
// `src/bin/client.rs`.
/// Well-known D-Bus name owned by the daemon.
pub const BUS_NAME: &str = "com.sw1nn.Stentor";

/// Object path that exposes the control interface.
pub const OBJECT_PATH: &str = "/com/sw1nn/Stentor";

/// D-Bus interface for daemon control operations.
pub const INTERFACE_NAME: &str = "com.sw1nn.Stentor.Control";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum, Default)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum MultiSlotHandler {
    #[default]
    None,
    Kitty,
}

impl fmt::Display for MultiSlotHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Kitty => "kitty",
        })
    }
}

/// Error returned when a `MultiSlotHandler` string is not recognised.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown multi-slot handler '{handler}' (expected 'none' or 'kitty')")]
pub struct MultiSlotHandlerParseError {
    handler: String,
}

impl FromStr for MultiSlotHandler {
    type Err = MultiSlotHandlerParseError;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "none" => Ok(Self::None),
            "kitty" => Ok(Self::Kitty),
            other => Err(MultiSlotHandlerParseError {
                handler: other.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonCommand {
    Start {
        unmute_source: bool,
        source: Option<String>,
        multi_slot_handler: MultiSlotHandler,
        /// Start a push-to-talk session: the VAD silence stop is disabled and
        /// the daemon arms the press/release state machine.
        ptt: bool,
    },
    Stop {
        /// Destination slot for the transcription. `None` sends to all active
        /// multi-slot destinations, or to slot 0 when there are none.
        command_slot: Option<usize>,
    },
    /// Release the push-to-talk key.
    PttUp,
    Quit,
}

struct ControlInterface {
    command_tx: mpsc::Sender<DaemonCommand>,
}

impl ControlInterface {
    async fn send(&self, command: DaemonCommand) -> zbus::fdo::Result<()> {
        self.command_tx
            .send(command)
            .await
            .map_err(|_| zbus::fdo::Error::Failed("daemon is shutting down".to_owned()))
    }
}

#[zbus::interface(name = "com.sw1nn.Stentor.Control")]
impl ControlInterface {
    /// Start a transcription session.
    ///
    /// An empty `source` uses the configured or PulseAudio default source.
    #[zbus(name = "Start")]
    async fn start(
        &self,
        unmute_source: bool,
        source: String,
        multi_slot_handler: String,
        ptt: bool,
    ) -> zbus::fdo::Result<()> {
        let multi_slot_handler =
            multi_slot_handler
                .parse()
                .map_err(|error: MultiSlotHandlerParseError| {
                    zbus::fdo::Error::InvalidArgs(error.to_string())
                })?;

        self.send(DaemonCommand::Start {
            unmute_source,
            source: (!source.is_empty()).then_some(source),
            multi_slot_handler,
            ptt,
        })
        .await
    }

    /// Stop recording. Without an explicit slot, sends to the focused
    /// multi-slot destination, all active destinations, or the default output.
    #[zbus(name = "Stop")]
    async fn stop(&self) -> zbus::fdo::Result<()> {
        self.send(DaemonCommand::Stop { command_slot: None }).await
    }

    /// Stop recording and send the transcription to a specific slot.
    #[zbus(name = "StopAtSlot")]
    async fn stop_at_slot(&self, command_slot: u32) -> zbus::fdo::Result<()> {
        let command_slot = usize::try_from(command_slot)
            .map_err(|_| zbus::fdo::Error::InvalidArgs(format!("invalid slot {command_slot}")))?;
        self.send(DaemonCommand::Stop {
            command_slot: Some(command_slot),
        })
        .await
    }

    /// Release the push-to-talk key.
    #[zbus(name = "PttUp")]
    async fn ptt_up(&self) -> zbus::fdo::Result<()> {
        self.send(DaemonCommand::PttUp).await
    }

    /// Liveness check: replies when the daemon is reachable on the bus.
    #[zbus(name = "Status")]
    async fn status(&self) -> String {
        "running".to_owned()
    }

    /// Ask the daemon to exit.
    #[zbus(name = "Quit")]
    async fn quit(&self) -> zbus::fdo::Result<()> {
        self.send(DaemonCommand::Quit).await
    }
}

/// Publish the control interface on the session bus.
///
/// The returned connection owns the bus name. Keep it alive for as long as the
/// daemon should be reachable.
pub async fn serve(command_tx: mpsc::Sender<DaemonCommand>) -> Result<zbus::Connection> {
    let connection = zbus::connection::Builder::session()
        .name(BUS_NAME)
        .allow_name_replacements(false)
        .replace_existing_names(false)
        .serve_at(OBJECT_PATH, ControlInterface { command_tx })
        .build()
        .await
        .with_context(|| format!("Failed to start D-Bus service {BUS_NAME} at {OBJECT_PATH}"))?;

    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "Daemon control interface ready"
    );

    Ok(connection)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_slot_handler_parse() -> Result<()> {
        assert_eq!("none".parse::<MultiSlotHandler>()?, MultiSlotHandler::None);
        assert_eq!(
            "kitty".parse::<MultiSlotHandler>()?,
            MultiSlotHandler::Kitty
        );
        Ok(())
    }

    #[test]
    fn test_multi_slot_handler_parse_rejects_unknown() {
        let result = "tmux".parse::<MultiSlotHandler>();
        let Err(error) = result else {
            panic!("expected an unknown handler error");
        };
        assert_eq!(
            error.to_string(),
            "unknown multi-slot handler 'tmux' (expected 'none' or 'kitty')"
        );
    }

    #[test]
    fn test_multi_slot_handler_display_round_trip() -> Result<()> {
        for handler in [MultiSlotHandler::None, MultiSlotHandler::Kitty] {
            let parsed = handler.to_string().parse::<MultiSlotHandler>()?;
            assert_eq!(parsed, handler);
        }
        Ok(())
    }

    #[test]
    fn test_dbus_names_are_valid() {
        assert!(zbus::names::BusName::try_from(BUS_NAME).is_ok());
        assert!(zbus::names::InterfaceName::try_from(INTERFACE_NAME).is_ok());
        assert!(zbus::ObjectPath::try_from(OBJECT_PATH).is_ok());
    }

    #[tokio::test]
    async fn test_stop_sends_command() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(1);
        let interface = ControlInterface { command_tx: tx };

        interface.stop().await?;

        assert_eq!(rx.try_recv()?, DaemonCommand::Stop { command_slot: None });
        Ok(())
    }

    #[tokio::test]
    async fn test_stop_at_slot_sends_command() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(1);
        let interface = ControlInterface { command_tx: tx };

        interface.stop_at_slot(3).await?;

        assert_eq!(
            rx.try_recv()?,
            DaemonCommand::Stop {
                command_slot: Some(3)
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_start_maps_empty_source_to_none() -> Result<()> {
        let (tx, mut rx) = mpsc::channel(1);
        let interface = ControlInterface { command_tx: tx };

        interface
            .start(false, String::new(), "kitty".to_owned(), false)
            .await?;

        assert_eq!(
            rx.try_recv()?,
            DaemonCommand::Start {
                unmute_source: false,
                source: None,
                multi_slot_handler: MultiSlotHandler::Kitty,
                ptt: false,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_send_reports_closed_channel() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let interface = ControlInterface { command_tx: tx };

        let result = interface.stop().await;

        assert!(matches!(
            result,
            Err(zbus::fdo::Error::Failed(message)) if message == "daemon is shutting down"
        ));
    }
}
