use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::future::Future;
use stentor::config::ClientConfig;
use stentor::daemon::{BUS_NAME, INTERFACE_NAME, MultiSlotHandler};

#[zbus::proxy(
    interface = "com.sw1nn.stentor.Control",
    default_service = "com.sw1nn.stentor",
    default_path = "/com/sw1nn/stentor"
)]
trait StentorControl {
    #[zbus(name = "Start")]
    async fn start(
        &self,
        unmute_source: bool,
        source: &str,
        multi_slot_handler: &str,
        ptt: bool,
    ) -> zbus::Result<()>;

    #[zbus(name = "Stop")]
    async fn stop(&self) -> zbus::Result<()>;

    #[zbus(name = "StopAtSlot")]
    async fn stop_at_slot(&self, command_slot: u32) -> zbus::Result<()>;

    #[zbus(name = "PttUp")]
    async fn ptt_up(&self) -> zbus::Result<()>;

    #[zbus(name = "Status")]
    async fn status(&self) -> zbus::Result<String>;

    #[zbus(name = "Quit")]
    async fn quit(&self) -> zbus::Result<()>;
}

#[derive(Subcommand)]
enum ClientCommand {
    /// Start recording (opens dialog and begins listening)
    Transcribe {
        #[command(flatten)]
        start: StartArgs,
    },
    /// Stop recording and finalize transcription
    TranscribeEnd {
        /// Destination slot for transcription (0 = default output, 1-8 = specific slots).
        /// Without this option, send to the focused kitty window when the kitty handler
        /// is active, else to slot 0.
        #[arg(long)]
        slot: Option<usize>,
    },
    /// Push-to-talk key down: start recording, or stop and send when latched
    PttDown {
        #[command(flatten)]
        start: StartArgs,
    },
    /// Push-to-talk key up: stop and send after a hold; ignored after a tap
    PttUp,
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

#[derive(clap::Args)]
struct StartArgs {
    /// Unmute source before recording
    #[arg(long)]
    unmute_source: bool,

    /// Audio source to use (default: from config or PulseAudio default)
    #[arg(long)]
    source: Option<String>,

    /// Multi-slot handler to use (none, kitty)
    #[arg(long)]
    multi_slot_handler: Option<MultiSlotHandler>,
}

struct StartParams {
    unmute_source: bool,
    source: Option<String>,
    multi_slot_handler: MultiSlotHandler,
    ptt: bool,
}

impl StartArgs {
    fn into_start_params(self, client_config: &ClientConfig, ptt: bool) -> StartParams {
        // Use CLI source if specified, otherwise fall back to config
        let source = self.source.or_else(|| client_config.source.clone());

        // Determine multi-slot handler from CLI arg or config
        let multi_slot_handler = self
            .multi_slot_handler
            .or_else(|| client_config.multi_slot_handler.clone())
            .unwrap_or(MultiSlotHandler::None);

        StartParams {
            unmute_source: self.unmute_source,
            source,
            multi_slot_handler,
            ptt,
        }
    }
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

  # Stop and auto-send to the focused kitty window (kitty mode) or default output
  stentorctl transcribe-end

  # Stop and auto-send to slot 1
  stentorctl transcribe-end --slot=1

  # Stop and auto-send to default output
  stentorctl transcribe-end --slot=0

  # Push-to-talk: press starts, hold and release stops and sends
  stentorctl ptt-down --unmute-source
  stentorctl ptt-up

  # A tap shorter than ptt-tap-threshold latches the session open;
  # the next ptt-down stops and sends.

Configuration can be set in $XDG_CONFIG_HOME/stentor/config.toml.
")]
struct Cli {
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
        start: StartArgs {
            unmute_source: false,
            source: None,
            multi_slot_handler: None,
        },
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

    // Load client config for default source and multi-slot handler
    let client_config = ClientConfig::load().context("Failed to load client configuration")?;

    let connection = zbus::Connection::session()
        .await
        .context("Failed to connect to the D-Bus session bus")?;
    let proxy = StentorControlProxy::new(&connection)
        .await
        .with_context(|| format!("Failed to create proxy for {BUS_NAME} ({INTERFACE_NAME})"))?;

    use ClientCommand::*;
    let response = match client_command {
        Transcribe { start } => {
            let params = start.into_start_params(&client_config, false);
            call_start(&proxy, params).await?;
            "OK: started"
        }
        TranscribeEnd { slot } => {
            call_stop(&proxy, slot).await?;
            "OK: stopped"
        }
        PttDown { start } => {
            let params = start.into_start_params(&client_config, true);
            call_start(&proxy, params).await?;
            "OK: started"
        }
        PttUp => {
            call(proxy.ptt_up(), "release push-to-talk").await?;
            "OK: released"
        }
        Status => {
            let status = call(proxy.status(), "query daemon status").await?;
            println!("OK: {status}");
            return Ok(());
        }
        Quit => {
            call(proxy.quit(), "quit daemon").await?;
            "OK: quitting"
        }
        ListSources => unreachable!("ListSources handled above"),
        Launch { .. } => unreachable!("Launch handled above"),
        Completions { .. } => unreachable!("Completions handled above"),
    };

    println!("{response}");
    Ok(())
}

async fn call_start(proxy: &StentorControlProxy<'_>, params: StartParams) -> Result<()> {
    let multi_slot_handler = params.multi_slot_handler.to_string();
    call(
        proxy.start(
            params.unmute_source,
            params.source.as_deref().unwrap_or_default(),
            &multi_slot_handler,
            params.ptt,
        ),
        "start transcription",
    )
    .await
}

async fn call_stop(proxy: &StentorControlProxy<'_>, slot: Option<usize>) -> Result<()> {
    match slot {
        Some(slot) => {
            let slot =
                u32::try_from(slot).with_context(|| format!("Slot {slot} is out of range"))?;
            call(proxy.stop_at_slot(slot), "stop transcription").await
        }
        None => call(proxy.stop(), "stop transcription").await,
    }
}

async fn call<T, F>(future: F, operation: &str) -> Result<T>
where
    F: Future<Output = zbus::Result<T>>,
{
    future.await.map_err(|error| {
        let context = match &error {
            zbus::Error::FDO(error)
                if matches!(
                    error.as_ref(),
                    zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_)
                ) =>
            {
                format!("Failed to {operation} via D-Bus ({BUS_NAME}). Is stentord running?")
            }
            _ => format!("Failed to {operation} via D-Bus ({BUS_NAME})"),
        };
        anyhow::Error::new(error).context(context)
    })
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
