use anyhow::{Context, Result};
use clap::Parser;
use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context as PulseContext, FlagSet as ContextFlagSet};
use libpulse_binding::mainloop::threaded::Mainloop;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use stentor::config::{ClientConfig, Config};
use stentor::daemon::DaemonCommand;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(name = "stentorctl")]
#[command(about = "Control transcription daemon", long_about = None)]
#[command(after_help = "Commands:
  start         Start recording (opens dialog and begins listening)
  stop          Stop recording (triggers transcription)
  quit          Quit the daemon
  list-sources  List available audio input sources

Examples:
  # Start recording
  stentorctl start

  # Start recording and unmute source
  stentorctl start --unmute-source

  # List available audio sources
  stentorctl list-sources

  # Start recording with specific source
  stentorctl start --source=\"USB Condenser Microphone\"

  # Stop recording and transcribe
  stentorctl stop

  # Shorthand (no subcommand = start)
  stentorctl

Configuration can be set in $XDG_CONFIG_HOME/stentor/config.toml.")]
struct Cli {
    /// Command to send (default: start)
    #[arg(value_parser = ["start", "stop", "quit", "list-sources"], default_value = "start")]
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .init();

    let cli = Cli::parse();
    let command_name = cli.command.unwrap_or_else(|| "start".to_string());

    // Handle list-sources command locally (doesn't need daemon)
    if command_name == "list-sources" {
        list_sources()?;
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
    let mut stream = UnixStream::connect(&socket_path)
        .await
        .with_context(|| {
            format!(
                "Failed to connect to daemon at {}. Is the daemon running?",
                socket_path.display()
            )
        })?;

    // Build command
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

fn list_sources() -> Result<()> {
    let mut mainloop = Mainloop::new().context("Failed to create PulseAudio mainloop")?;
    let mut context = PulseContext::new(&mainloop, "stentor-list-sources")
        .context("Failed to create PulseAudio context")?;

    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .context("Failed to connect to PulseAudio")?;

    mainloop.lock();
    mainloop.start().context("Failed to start mainloop")?;

    // Wait for context to be ready
    loop {
        match context.get_state() {
            libpulse_binding::context::State::Ready => break,
            libpulse_binding::context::State::Failed
            | libpulse_binding::context::State::Terminated => {
                mainloop.unlock();
                mainloop.stop();
                anyhow::bail!("PulseAudio context failed");
            }
            _ => {
                mainloop.unlock();
                std::thread::sleep(std::time::Duration::from_millis(10));
                mainloop.lock();
            }
        }
    }

    // Collect source names and find default
    let sources = Rc::new(RefCell::new(Vec::new()));
    let sources_clone = Rc::clone(&sources);

    let introspect = context.introspect();
    introspect.get_source_info_list(move |result| match result {
        ListResult::Item(source_info) => {
            if let Some(name) = source_info.name.as_ref() {
                sources_clone.borrow_mut().push(name.to_string());
            }
        }
        ListResult::End => {}
        ListResult::Error => {}
    });

    mainloop.unlock();
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Get default source
    let default_source = Rc::new(RefCell::new(None));
    let default_source_clone = Rc::clone(&default_source);

    mainloop.lock();
    let introspect = context.introspect();
    introspect.get_server_info(move |server_info| {
        if let Some(default) = server_info.default_source_name.as_ref() {
            *default_source_clone.borrow_mut() = Some(default.to_string());
        }
    });
    mainloop.unlock();

    std::thread::sleep(std::time::Duration::from_millis(100));

    mainloop.lock();
    let source_names = sources.borrow().clone();
    let default_name = default_source.borrow().clone();
    mainloop.unlock();

    mainloop.stop();

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
    println!("  stentorctl start --source=\"SOURCE_NAME\"");
    println!("\nOr add to ~/.config/stentor/config.toml:");
    println!("  [client]");
    println!("  source = \"SOURCE_NAME\"");

    Ok(())
}
