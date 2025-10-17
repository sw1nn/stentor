use anyhow::{Context, Result};
use clap::Parser;
use libadwaita as adw;
use std::path::PathBuf;
use std::sync::Arc;

use stentor::config::{Config, ConfigFile};
use stentor::transcription::Transcriber;

#[derive(Parser)]
#[command(name = "stentord")]
#[command(about = "Real-time transcription daemon", long_about = None)]
#[command(
    after_help = "Configuration can be set in $XDG_CONFIG_HOME/stentor/config.toml.\nCommand-line arguments override config file settings."
)]
struct Cli {
    /// Whisper model size
    #[arg(long, value_parser = ["tiny", "base", "small", "medium", "large"])]
    model: Option<String>,

    /// Language code for transcription (default: from config or 'en')
    #[arg(long)]
    language: Option<String>,

    /// Unix socket path (default: $XDG_RUNTIME_DIR/stentor.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Command to execute with transcribed text. Use $TRANSCRIPTION environment variable.
    #[arg(long)]
    output_command: Option<String>,

    /// Seconds of silence before stopping recording (default: from config or 1.5)
    #[arg(long)]
    silence_duration: Option<f32>,

    /// Disable logging output (overrides RUST_LOG)
    #[arg(short, long)]
    quiet: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Parse CLI arguments first to check for --quiet flag
    let cli = Cli::parse();

    // Initialize logging with appropriate level
    // If --quiet is set, disable logging completely
    // Otherwise, default to info level if RUST_LOG is not set
    if !cli.quiet {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
    }

    // Initialize GTK and libadwaita
    adw::init().context("Failed to initialize libadwaita")?;

    // Load configuration
    let mut config = Config::load().context("Failed to load configuration")?;
    let config_file = ConfigFile::load().context("Failed to load full configuration")?;

    // Override config with CLI arguments
    if let Some(model) = cli.model {
        config.model = model;
    }
    if let Some(language) = cli.language {
        config.language = language;
    }
    if let Some(output_command) = cli.output_command {
        config.output_command = Some(output_command);
    }
    if let Some(silence_duration) = cli.silence_duration {
        config.silence_duration = silence_duration;
    }
    tracing::info!("Loaded configuration: {:?}", config);
    tracing::info!(
        "Using silence_duration: {} seconds",
        config.silence_duration
    );

    // Log build-time compilation flags for debugging
    tracing::info!("Build-time flags:");
    tracing::info!("  Profile: {}", env!("BUILD_PROFILE"));
    tracing::info!("  Opt Level: {}", env!("BUILD_OPT_LEVEL"));
    tracing::info!("  Debug: {}", env!("BUILD_DEBUG"));
    tracing::info!("  CFLAGS: {}", env!("BUILD_CFLAGS"));
    tracing::info!("  CXXFLAGS: {}", env!("BUILD_CXXFLAGS"));
    tracing::info!("  LDFLAGS: {}", env!("BUILD_LDFLAGS"));
    tracing::info!("  RUSTFLAGS: {}", env!("BUILD_RUSTFLAGS"));
    tracing::info!(
        "  CARGO_PROFILE_RELEASE_LTO: {}",
        env!("BUILD_CARGO_PROFILE_RELEASE_LTO")
    );
    tracing::info!(
        "  CARGO_PROFILE_RELEASE_CODEGEN_UNITS: {}",
        env!("BUILD_CARGO_PROFILE_RELEASE_CODEGEN_UNITS")
    );

    // Load Whisper model (this will download if not present)
    tracing::info!("Loading Whisper model: {}", config.model);
    let model = config.model.clone();
    let language = config.language.clone();
    let transcriber = tokio::task::spawn_blocking(move || Transcriber::new(model, language))
        .await
        .context("Failed to spawn model loading task")?
        .context("Failed to load Whisper model")?;
    let transcriber = Arc::new(transcriber);
    tracing::info!("Whisper model loaded successfully");

    // Determine socket path
    let socket_path = if let Some(custom_socket) = cli.socket {
        custom_socket
    } else {
        config.socket_path()?
    };

    // Run daemon application
    stentor::daemon_app::run(config, config_file, transcriber, socket_path).await
}
