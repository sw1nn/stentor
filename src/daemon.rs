use anyhow::{Context, Result};
use std::path::PathBuf;
use std::str::FromStr;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonCommand {
    Start,
    Stop,
    Status,
    Quit,
}

impl FromStr for DaemonCommand {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim() {
            "start" => Ok(DaemonCommand::Start),
            "stop" => Ok(DaemonCommand::Stop),
            "status" => Ok(DaemonCommand::Status),
            "quit" => Ok(DaemonCommand::Quit),
            _ => Err(format!("Unknown command: {}", s)),
        }
    }
}

impl DaemonCommand {
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            DaemonCommand::Start => "start",
            DaemonCommand::Stop => "stop",
            DaemonCommand::Status => "status",
            DaemonCommand::Quit => "quit",
        }
    }
}

pub struct DaemonServer {
    socket_path: PathBuf,
    listener: Option<UnixListener>,
}

impl DaemonServer {
    pub fn new(socket_path: PathBuf) -> Result<Self> {
        // Remove existing socket if it exists
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .with_context(|| format!("Failed to remove existing socket: {}", socket_path.display()))?;
        }

        Ok(Self {
            socket_path,
            listener: None,
        })
    }

    pub async fn bind(&mut self) -> Result<()> {
        let listener = UnixListener::bind(&self.socket_path)
            .with_context(|| format!("Failed to bind to socket: {}", self.socket_path.display()))?;

        log::info!("Daemon listening on: {}", self.socket_path.display());
        self.listener = Some(listener);

        Ok(())
    }

    pub async fn run(
        &mut self,
        command_tx: mpsc::Sender<DaemonCommand>,
    ) -> Result<()> {
        let listener = self.listener.as_ref()
            .context("Server not bound - call bind() first")?;

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let command_tx = command_tx.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(stream, command_tx).await {
                            log::error!("Error handling client: {}", e);
                        }
                    });
                }
                Err(e) => {
                    log::error!("Error accepting connection: {}", e);
                }
            }
        }
    }
}

impl Drop for DaemonServer {
    fn drop(&mut self) {
        if self.socket_path.exists() {
            if let Err(e) = std::fs::remove_file(&self.socket_path) {
                log::error!("Failed to remove socket file: {}", e);
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

        if let Ok(command) = command_str.parse::<DaemonCommand>() {
            log::info!("Received command: {:?}", command);

            // Send command to main loop
            if command_tx.send(command.clone()).await.is_err() {
                log::error!("Failed to send command (receiver dropped)");
                writer.write_all(b"ERROR: daemon shutting down\n").await?;
                break;
            }

            // Send response
            let response = match command {
                DaemonCommand::Start => "OK: started\n",
                DaemonCommand::Stop => "OK: stopped\n",
                DaemonCommand::Status => "OK: running\n",
                DaemonCommand::Quit => {
                    writer.write_all(b"OK: quitting\n").await?;
                    break;
                }
            };

            writer.write_all(response.as_bytes()).await?;
        } else {
            log::warn!("Unknown command: {}", command_str);
            writer.write_all(b"ERROR: unknown command\n").await?;
        }

        line.clear();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_parsing() {
        assert_eq!("start".parse::<DaemonCommand>(), Ok(DaemonCommand::Start));
        assert_eq!("stop".parse::<DaemonCommand>(), Ok(DaemonCommand::Stop));
        assert_eq!("status".parse::<DaemonCommand>(), Ok(DaemonCommand::Status));
        assert_eq!("quit".parse::<DaemonCommand>(), Ok(DaemonCommand::Quit));
        assert!("invalid".parse::<DaemonCommand>().is_err());
    }

    #[test]
    fn test_command_as_str() {
        assert_eq!(DaemonCommand::Start.as_str(), "start");
        assert_eq!(DaemonCommand::Stop.as_str(), "stop");
        assert_eq!(DaemonCommand::Status.as_str(), "status");
        assert_eq!(DaemonCommand::Quit.as_str(), "quit");
    }
}
