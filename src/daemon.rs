use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
pub enum MultiSlotHandler {
    None,
    Kitty,
}

impl Default for MultiSlotHandler {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DaemonCommand {
    Start {
        #[serde(default)]
        unmute_source: bool,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        multi_slot_handler: MultiSlotHandler,
    },
    Stop {
        /// Destination slot to send transcription to (0 = default, 1-4 = specific slots)
        #[serde(default)]
        command_slot: usize,
    },
    Status,
    Quit,
}

pub struct DaemonServer {
    socket_path: PathBuf,
    listener: Option<UnixListener>,
}

impl DaemonServer {
    pub fn new(socket_path: PathBuf) -> Result<Self> {
        // Remove existing socket if it exists and is actually a socket
        if socket_path.exists() {
            let metadata = std::fs::metadata(&socket_path).with_context(|| {
                format!("Failed to get metadata for: {}", socket_path.display())
            })?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::FileTypeExt;
                if metadata.file_type().is_socket() {
                    std::fs::remove_file(&socket_path).with_context(|| {
                        format!(
                            "Failed to remove existing socket: {}",
                            socket_path.display()
                        )
                    })?;
                } else {
                    anyhow::bail!("Path exists but is not a socket: {}", socket_path.display());
                }
            }

            #[cfg(not(unix))]
            {
                std::fs::remove_file(&socket_path).with_context(|| {
                    format!(
                        "Failed to remove existing socket: {}",
                        socket_path.display()
                    )
                })?;
            }
        }

        Ok(Self {
            socket_path,
            listener: None,
        })
    }

    pub async fn bind(&mut self) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("Failed to bind to socket: {}", self.socket_path.display()))?;

        tracing::info!("Daemon listening on: {}", self.socket_path.display());
        self.listener = Some(listener);

        Ok(())
    }

    pub async fn run(&mut self, command_tx: mpsc::Sender<DaemonCommand>) -> Result<()> {
        let listener = self
            .listener
            .as_ref()
            .context("Server not bound - call bind() first")?;

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let command_tx = command_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, command_tx).await {
                            tracing::error!("Error handling client: {}", e);
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("Error accepting connection: {}", e);
                }
            }
        }
    }
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        if self.socket_path.exists() {
            // Check if it's actually a socket before removing
            if let Ok(metadata) = std::fs::metadata(&self.socket_path) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if metadata.file_type().is_socket() {
                        if let Err(e) = std::fs::remove_file(&self.socket_path) {
                            tracing::error!("Failed to remove socket file: {}", e);
                        }
                    } else {
                        tracing::warn!(
                            "Path exists but is not a socket, not removing: {}",
                            self.socket_path.display()
                        );
                    }
                }

                #[cfg(not(unix))]
                {
                    if let Err(e) = std::fs::remove_file(&self.socket_path) {
                        tracing::error!("Failed to remove socket file: {}", e);
                    }
                }
            }
        }
    }
}

async fn handle_client(
    mut stream: UnixStream,
    command_tx: mpsc::Sender<DaemonCommand>,
) -> Result<()> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    while reader.read_line(&mut line).await? > 0 {
        let command_str = line.trim();

        // Try to parse as JSON
        match serde_json::from_str::<DaemonCommand>(command_str) {
            Ok(command) => {
                tracing::info!("Received command: {:?}", command);

                // Send command to main loop
                if command_tx.send(command.clone()).await.is_err() {
                    tracing::error!("Failed to send command (receiver dropped)");
                    writer.write_all(b"ERROR: daemon shutting down\n").await?;
                    break;
                }

                // Send response
                use DaemonCommand::*;
                let response = match command {
                    Start { .. } => "OK: started\n",
                    Stop { .. } => "OK: stopped\n",
                    Status => "OK: running\n",
                    Quit => {
                        writer.write_all(b"OK: quitting\n").await?;
                        break;
                    }
                };

                writer.write_all(response.as_bytes()).await?;
            }
            Err(e) => {
                tracing::warn!("Failed to parse command '{}': {}", command_str, e);
                writer
                    .write_all(format!("ERROR: invalid command: {}\n", e).as_bytes())
                    .await?;
            }
        }

        line.clear();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_serialization() {
        // Test Start with no options
        let cmd = DaemonCommand::Start {
            unmute_source: false,
            source: None,
            multi_slot_handler: MultiSlotHandler::None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: DaemonCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, parsed);

        // Test Start with unmute_source
        let cmd = DaemonCommand::Start {
            unmute_source: true,
            source: None,
            multi_slot_handler: MultiSlotHandler::None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: DaemonCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, parsed);

        // Test Start with source (including spaces)
        let cmd = DaemonCommand::Start {
            unmute_source: false,
            source: Some("USB Condenser Microphone".to_string()),
            multi_slot_handler: MultiSlotHandler::None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: DaemonCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, parsed);

        // Test Start with Kitty handler
        let cmd = DaemonCommand::Start {
            unmute_source: true,
            source: Some("HD-Audio Generic".to_string()),
            multi_slot_handler: MultiSlotHandler::Kitty,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: DaemonCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, parsed);

        // Test Stop with different slots
        let cmd = DaemonCommand::Stop { command_slot: 0 };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: DaemonCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, parsed);

        let cmd = DaemonCommand::Stop { command_slot: 2 };
        let json = serde_json::to_string(&cmd).unwrap();
        let parsed: DaemonCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, parsed);

        // Test other commands
        let commands = vec![DaemonCommand::Status, DaemonCommand::Quit];

        for cmd in commands {
            let json = serde_json::to_string(&cmd).unwrap();
            let parsed: DaemonCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, parsed);
        }
    }
}
