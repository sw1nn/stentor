use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use sw1nn_transcription::config::Config;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "sw1nn-transcription")]
#[command(about = "Control transcription daemon", long_about = None)]
#[command(after_help = "Commands:
  start     Start recording (opens dialog and begins listening)
  stop      Stop recording (triggers transcription)
  quit      Quit the daemon

Examples:
  # Start recording
  sw1nn-transcription start

  # Stop recording and transcribe
  sw1nn-transcription stop

  # Shorthand (no subcommand = start)
  sw1nn-transcription

Configuration can be set in $XDG_CONFIG_HOME/sw1nn-transcription/config.toml.")]
struct Cli {
    /// Command to send (default: start)
    #[arg(value_parser = ["start", "stop", "quit"], default_value = "start")]
    command: Option<String>,

    /// Unix socket path (default: from config or $XDG_RUNTIME_DIR/sw1nn-transcription.sock)
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    // Load config to get socket path
    let config = Config::load().context("Failed to load configuration")?;

    let socket_path = if let Some(custom_socket) = cli.socket {
        custom_socket
    } else {
        config.socket_path()?
    };

    // Connect to daemon
    let mut stream = UnixStream::connect(&socket_path)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to daemon at {}. Is the daemon running?",
                socket_path.display()
            )
        })?;

    // Send command
    let command = cli.command.unwrap_or_else(|| "start".to_string());

    stream
        .write_all(format!("{}\n", command).as_bytes())
        .await
        .context("Failed to send command")?;

    // Read response
    let (reader, _writer) = stream.split();
    let mut reader = BufReader::new(reader);
    let mut response = String::new();

    reader
        .read_line(&mut response)
        .await
        .context("Failed to read response")?;

    // Print response
    print!("{}", response);

    // Check if response indicates error
    if response.starts_with("ERROR") {
        std::process::exit(1);
    }

    Ok(())
}
