use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use xdg::BaseDirectories;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigFile {
    #[serde(default)]
    pub daemon: Config,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

fn default_model() -> String {
    "base".to_string()
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
    "sw1nn-transcription.sock".to_string()
}


impl Config {
    /// Load configuration from XDG config directory
    pub fn load() -> Result<Self> {
        let xdg_dirs = BaseDirectories::with_prefix("sw1nn-transcription")
            .context("Failed to initialize XDG directories")?;

        // Try to find config file
        if let Some(config_path) = xdg_dirs.find_config_file("config.toml") {
            Self::load_from_file(&config_path)
        } else {
            log::info!("No config file found, using defaults");
            Ok(Self::default())
        }
    }

    /// Load configuration from a specific file
    pub fn load_from_file(path: &PathBuf) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config_file: ConfigFile = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        log::info!("Loaded config from: {}", path.display());
        Ok(config_file.daemon)
    }

    /// Get the path to the config file (creates directory if needed)
    #[allow(dead_code)]
    pub fn config_path() -> Result<PathBuf> {
        let xdg_dirs = BaseDirectories::with_prefix("sw1nn-transcription")
            .context("Failed to initialize XDG directories")?;

        xdg_dirs
            .place_config_file("config.toml")
            .context("Failed to determine config file path")
    }

    /// Get the socket path in XDG runtime directory
    pub fn socket_path(&self) -> Result<PathBuf> {
        let xdg_dirs = BaseDirectories::new().context("Failed to initialize XDG directories")?;

        let runtime_dir = xdg_dirs
            .get_runtime_directory()
            .context("No XDG_RUNTIME_DIR available")?;

        Ok(runtime_dir.join(&self.socket_name))
    }

    /// Save configuration to file
    #[allow(dead_code)]
    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;

        let contents = toml::to_string_pretty(self)
            .context("Failed to serialize configuration")?;

        std::fs::write(&path, contents)
            .with_context(|| format!("Failed to write config file: {}", path.display()))?;

        log::info!("Saved config to: {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.model, "base");
        assert_eq!(config.language, "en");
        assert_eq!(config.silence_duration, 1.5);
        assert_eq!(config.silence_threshold, 0.01);
        assert_eq!(config.min_speech_duration, 0.5);
        assert!(config.output_command.is_none());
        assert_eq!(config.socket_name, "sw1nn-transcription.sock");
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
        assert_eq!(config.output_command, Some("tmux load-buffer -".to_string()));
    }
}
