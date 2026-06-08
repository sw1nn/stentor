use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use stentor::config::{ClientConfig, Config};
use stentor::daemon::{DaemonCommand, MultiSlotHandler};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Subcommand)]
enum ClientCommand {
    /// Start recording (opens dialog and begins listening)
    Transcribe {
        /// Unmute source before recording
        #[arg(long)]
        unmute_source: bool,

        /// Audio source to use (default: from config or PulseAudio default)
        #[arg(long)]
        source: Option<String>,

        /// Multi-slot handler to use (none, kitty)
        #[arg(long)]
        multi_slot_handler: Option<MultiSlotHandler>,
    },
    /// Stop recording and finalize transcription
    TranscribeEnd {
        /// Destination slot for transcription (0 = default, 1-4 = specific slots)
        #[arg(long, default_value = "0")]
        slot: usize,
    },
    /// Launch a new terminal window with STENTOR_SLOT env var
    Launch {
        /// Command to run in the new window (defaults to shell if not specified)
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Query daemon status
    Status,
    /// Quit the daemon
    Quit,
    /// List available audio input sources
    ListSources,
    /// Generate shell completions
    Completions { shell: clap_complete::Shell },
}

#[derive(Parser)]
#[command(name = "stentorctl")]
#[command(version)]
#[command(about = "Control transcription daemon", long_about = None)]
#[command(after_help = "Examples:
  # Start recording
  stentorctl transcribe

  # Start recording and unmute source
  stentorctl transcribe --unmute-source

  # List available audio sources
  stentorctl list-sources

  # Start recording with specific source
  stentorctl transcribe --source=\"USB Condenser Microphone\"

  # Start recording with Kitty multi-slot handler
  stentorctl transcribe --multi-slot-handler=kitty

  # Stop recording and finalize transcription (shows dialog)
  stentorctl transcribe-end

  # Stop and auto-send to slot 1
  stentorctl transcribe-end --slot=1

  # Stop and auto-send to default output
  stentorctl transcribe-end --slot=0

Configuration can be set in $XDG_CONFIG_HOME/stentor/config.toml.
")]
struct Cli {
    /// Unix socket path (default: from config or $XDG_RUNTIME_DIR/stentor.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<ClientCommand>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    // Default to Transcribe if no subcommand provided
    let client_command = cli.command.unwrap_or(ClientCommand::Transcribe {
        unmute_source: false,
        source: None,
        multi_slot_handler: None,
    });

    // Handle list-sources command locally (doesn't need daemon)
    if matches!(client_command, ClientCommand::ListSources) {
        list_sources()?;
        return Ok(());
    }

    // Handle launch command locally (doesn't need daemon)
    if let ClientCommand::Launch { command } = &client_command {
        return launch_command(command);
    }

    // Handle completions generation locally (doesn't need daemon)
    if let ClientCommand::Completions { shell } = client_command {
        use clap::CommandFactory;
        clap_complete::generate(
            shell,
            &mut Cli::command(),
            "stentorctl",
            &mut std::io::stdout(),
        );
        return Ok(());
    }

    // Load configs to get socket path and client settings
    let config = Config::load().context("Failed to load configuration")?;
    let client_config = ClientConfig::load().context("Failed to load client configuration")?;

    let socket_path = if let Some(custom_socket) = cli.socket {
        custom_socket
    } else {
        config.socket_path()?
    };

    // Connect to daemon
    let mut stream = UnixStream::connect(&socket_path).await.with_context(|| {
        format!(
            "Failed to connect to daemon at {}. Is the daemon running?",
            socket_path.display()
        )
    })?;

    // Build daemon command from client command
    use ClientCommand::*;
    let command = match client_command {
        Transcribe {
            unmute_source,
            source,
            multi_slot_handler,
        } => {
            // Use CLI source if specified, otherwise fall back to config
            let source = source.or(client_config.source);

            // Determine multi-slot handler from CLI arg or config
            let multi_slot_handler = multi_slot_handler
                .or(client_config.multi_slot_handler)
                .unwrap_or(MultiSlotHandler::None);

            DaemonCommand::Start {
                unmute_source,
                source,
                multi_slot_handler,
            }
        }
        TranscribeEnd { slot } => DaemonCommand::Stop { command_slot: slot },
        Status => DaemonCommand::Status,
        Quit => DaemonCommand::Quit,
        ListSources => unreachable!("ListSources handled above"),
        Launch { .. } => unreachable!("Launch handled above"),
        Completions { .. } => unreachable!("Completions handled above"),
    };

    // Serialize to JSON
    let command_str =
        serde_json::to_string(&command).context("Failed to serialize command")? + "\n";

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

fn list_sources() -> Result<()> {
    use stentor::audio::PulseIntrospector;

    let mut introspector =
        PulseIntrospector::new().context("Failed to create PulseAudio introspector")?;

    let source_names = introspector
        .list_sources()
        .context("Failed to list audio sources")?;
    let default_name = introspector
        .get_default_source_name()
        .context("Failed to get default source")?;

    println!("Available audio input sources:");
    println!("==============================\n");

    if source_names.is_empty() {
        println!("No input sources found");
        return Ok(());
    }

    for name in &source_names {
        let is_default = default_name.as_ref().map(|dn| dn == name).unwrap_or(false);
        if is_default {
            println!("  {} (default)", name);
        } else {
            println!("  {}", name);
        }
    }

    println!("\nUsage:");
    println!("  stentorctl transcribe --source=\"SOURCE_NAME\"");
    println!("\nOr add to ~/.config/stentor/config.toml:");
    println!("  [client]");
    println!("  source = \"SOURCE_NAME\"");

    Ok(())
}

fn launch_command(command: &[String]) -> Result<()> {
    use stentor::kitty;

    // Find next available slot
    let slot = kitty::find_available_slot().context("Failed to discover available slots")?;

    let slot = match slot {
        Some(s) => s,
        None => {
            eprintln!(
                "ERROR: All slots (1-8) are occupied. Close a stentor window to free a slot."
            );
            std::process::exit(1);
        }
    };

    // Launch window with the slot
    kitty::launch_with_slot(slot, command).context("Failed to launch kitty window")?;

    println!("Launched window in slot {}", slot);
    Ok(())
}
