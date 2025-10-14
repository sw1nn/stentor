use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use stentor::config::{ClientConfig, Config};
use stentor::daemon::DaemonCommand;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "stentorctl")]
#[command(about = "Control transcription daemon", long_about = None)]
#[command(after_help = "Commands:
  start     Start recording (opens dialog and begins listening)
  stop      Stop recording (triggers transcription)
  quit      Quit the daemon

Examples:
  # Start recording
  stentorctl start

  # Start recording and unmute source
  stentorctl start --unmute-source

  # Start recording with specific source
  stentorctl start --source=alsa_input.usb

  # Stop recording and transcribe
  stentorctl stop

  # Shorthand (no subcommand = start)
  stentorctl

Configuration can be set in $XDG_CONFIG_HOME/stentor/config.toml.")]
struct Cli {
    /// Command to send (default: start)
    #[arg(value_parser = ["start", "stop", "quit"], default_value = "start")]
    command: Option<String>,

    /// Unmute source before recording (only applies to 'start' command)
    #[arg(long)]
    unmute_source: bool,

    /// Audio source to use (default: from config or PulseAudio default)
    #[arg(long)]
    source: Option<String>,

    /// Unix socket path (default: from config or $XDG_RUNTIME_DIR/stentor.sock)
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();

    // Load configs to get socket path and client settings
    let config = Config::load().context("Failed to load configuration")?;
    let client_config = ClientConfig::load().context("Failed to load client configuration")?;

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

    // Build command
    let command_name = cli.command.unwrap_or_else(|| "start".to_string());
    let command = match command_name.as_str() {
        "start" => {
            // Use CLI source if specified, otherwise fall back to config
            let source = cli.source.or(client_config.source);
            DaemonCommand::Start {
                unmute_source: cli.unmute_source,
                source,
            }
        }
        "stop" => DaemonCommand::Stop,
        "status" => DaemonCommand::Status,
        "quit" => DaemonCommand::Quit,
        _ => anyhow::bail!("Unknown command: {}", command_name),
    };

    // Serialize to JSON
    let command_str = serde_json::to_string(&command)
        .context("Failed to serialize command")?
        + "\n";

    stream
        .write_all(command_str.as_bytes())
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
