use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use xdg::BaseDirectories;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub daemon: Config,
    #[serde(default)]
    pub client: ClientConfig,
    #[serde(default)]
    pub kitty: KittyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KittyConfig {
    #[serde(default, alias = "background-color-cmd")]
    pub background_color_cmd: Option<String>,

    #[serde(
        default = "default_kitty_base_background",
        alias = "base-background-color"
    )]
    pub base_background_color: String,

    #[serde(default, alias = "output-command")]
    pub output_command: Option<String>,
}

impl Default for KittyConfig {
    fn default() -> Self {
        Self {
            background_color_cmd: None,
            base_background_color: default_kitty_base_background(),
            output_command: None,
        }
    }
}

fn default_kitty_base_background() -> String {
    "#1e1e2e".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub source: Option<String>,

    #[serde(default, alias = "multi-slot-handler")]
    pub multi_slot_handler: Option<crate::daemon::MultiSlotHandler>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            source: None,
            multi_slot_handler: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_model")]
    pub model: String,

    #[serde(default = "default_language")]
    pub language: String,

    #[serde(default = "default_silence_duration", alias = "silence-duration")]
    pub silence_duration: f32,

    #[serde(default = "default_silence_threshold", alias = "silence-threshold")]
    pub silence_threshold: f32,

    #[serde(default = "default_min_speech_duration", alias = "min-speech-duration")]
    pub min_speech_duration: f32,

    #[serde(default, alias = "output-command")]
    pub output_command: Option<String>,

    #[serde(default = "default_socket_name", alias = "socket-name")]
    pub socket_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: default_model(),
            language: default_language(),
            silence_duration: default_silence_duration(),
            silence_threshold: default_silence_threshold(),
            min_speech_duration: default_min_speech_duration(),
            output_command: None,
            socket_name: default_socket_name(),
        }
    }
}

fn default_model() -> String {
    "tiny".to_string()
}

fn default_language() -> String {
    "en".to_string()
}

fn default_silence_duration() -> f32 {
    1.5
}

fn default_silence_threshold() -> f32 {
    0.01
}

fn default_min_speech_duration() -> f32 {
    0.5
}

fn default_socket_name() -> String {
    "stentor.sock".to_string()
}

impl ConfigFile {
    /// Load full configuration file from XDG config directory
    pub fn load() -> Result<Self> {
        let xdg_dirs = BaseDirectories::with_prefix("stentor");

        // Try to find config file
        if let Some(config_path) = xdg_dirs.find_config_file("config.toml") {
            let contents = std::fs::read_to_string(&config_path).with_context(|| {
                format!("Failed to read config file: {}", config_path.display())
            })?;

            let config_file: ConfigFile = toml::from_str(&contents).with_context(|| {
                format!("Failed to parse config file: {}", config_path.display())
            })?;

            Ok(config_file)
        } else {
            tracing::info!("No config file found, using defaults");
            Ok(Self::default())
        }
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            daemon: Config::default(),
            client: ClientConfig::default(),
            kitty: KittyConfig::default(),
        }
    }
}

impl ClientConfig {
    /// Load client configuration from XDG config directory
    #[allow(dead_code)] // Used in client binary
    pub fn load() -> Result<Self> {
        let xdg_dirs = BaseDirectories::with_prefix("stentor");

        // Try to find config file
        if let Some(config_path) = xdg_dirs.find_config_file("config.toml") {
            let contents = std::fs::read_to_string(&config_path).with_context(|| {
                format!("Failed to read config file: {}", config_path.display())
            })?;

            let config_file: ConfigFile = toml::from_str(&contents).with_context(|| {
                format!("Failed to parse config file: {}", config_path.display())
            })?;

            Ok(config_file.client)
        } else {
            tracing::info!("No config file found, using defaults");
            Ok(Self::default())
        }
    }
}

impl Config {
    /// Load configuration from XDG config directory
    pub fn load() -> Result<Self> {
        let xdg_dirs = BaseDirectories::with_prefix("stentor");

        // Try to find config file
        let config = if let Some(config_path) = xdg_dirs.find_config_file("config.toml") {
            Self::load_from_file(&config_path)?
        } else {
            tracing::info!("No config file found, using defaults");
            Self::default()
        };

        // Warn if socket name doesn't end in .sock
        if !config.socket_name.ends_with(".sock") {
            tracing::warn!(
                "Socket name '{}' does not end with .sock - this may cause issues with some tools",
                config.socket_name
            );
        }

        Ok(config)
    }

    /// Load configuration from a specific file
    pub fn load_from_file(path: &PathBuf) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config_file: ConfigFile = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        tracing::info!("Loaded config from: {}", path.display());
        Ok(config_file.daemon)
    }

    /// Get the path to the config file (creates directory if needed)
    #[allow(dead_code)]
    pub fn config_path() -> Result<PathBuf> {
        let xdg_dirs = BaseDirectories::with_prefix("stentor");

        xdg_dirs
            .place_config_file("config.toml")
            .context("Failed to determine config file path")
    }

    /// Get the socket path in XDG runtime directory
    pub fn socket_path(&self) -> Result<PathBuf> {
        let xdg_dirs = BaseDirectories::new();

        let runtime_dir = xdg_dirs
            .get_runtime_directory()
            .context("No XDG_RUNTIME_DIR available")?;

        Ok(runtime_dir.join(&self.socket_name))
    }

    /// Save configuration to file
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;

        let contents = toml::to_string_pretty(self).context("Failed to serialize configuration")?;

        std::fs::write(&path, contents)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;

        tracing::info!("Saved config to: {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.model, "tiny");
        assert_eq!(config.language, "en");
        assert_eq!(config.silence_duration, 1.5);
        assert_eq!(config.silence_threshold, 0.01);
        assert_eq!(config.min_speech_duration, 0.5);
        assert!(config.output_command.is_none());
        assert_eq!(config.socket_name, "stentor.sock");
    }

    #[test]
    fn test_config_serialization() {
        let config = Config::default();
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(config.model, parsed.model);
        assert_eq!(config.language, parsed.language);
    }

    #[test]
    fn test_partial_config() {
        let toml_str = r#"
            model = "tiny"
            output_command = "tmux load-buffer -"
        "#;

        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model, "tiny");
        assert_eq!(config.language, "en"); // default
        assert_eq!(
            config.output_command,
            Some("tmux load-buffer -".to_string())
        );
    }
}
