use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use xdg::BaseDirectories;

/// Represents a pattern for matching windows to slots.
/// Parsed from config strings with prefix notation.
#[derive(Debug, Clone)]
pub enum SlotMatcher {
    /// Match against foreground process command name (basename)
    /// Format: "cmd:name" or just "name" (for backward compatibility)
    Cmd(String),
    /// Match against Hyprland window title using regex
    /// Format: "hypr-title:regex"
    HyprTitle(Regex),
    /// Match against Hyprland window class using regex
    /// Format: "hypr-class:regex"
    HyprClass(Regex),
}

impl SlotMatcher {
    /// Parse a slot match pattern from a config string.
    pub fn parse(s: &str) -> Result<Self> {
        if let Some(pattern) = s.strip_prefix("cmd:") {
            Ok(SlotMatcher::Cmd(pattern.to_string()))
        } else if let Some(pattern) = s.strip_prefix("hypr-title:") {
            let regex = Regex::new(pattern)
                .with_context(|| format!("Invalid regex in hypr-title pattern: {pattern}"))?;
            Ok(SlotMatcher::HyprTitle(regex))
        } else if let Some(pattern) = s.strip_prefix("hypr-class:") {
            let regex = Regex::new(pattern)
                .with_context(|| format!("Invalid regex in hypr-class pattern: {pattern}"))?;
            Ok(SlotMatcher::HyprClass(regex))
        } else {
            // Backward compatibility: treat bare strings as cmd matches
            Ok(SlotMatcher::Cmd(s.to_string()))
        }
    }

    /// Check if this matcher matches a given command name (for Kitty foreground process).
    pub fn matches_cmd(&self, cmd_name: &str) -> bool {
        match self {
            SlotMatcher::Cmd(pattern) => cmd_name == pattern,
            _ => false,
        }
    }

    /// Check if this is a Hyprland matcher (title or class).
    pub fn is_hyprland_matcher(&self) -> bool {
        matches!(self, SlotMatcher::HyprTitle(_) | SlotMatcher::HyprClass(_))
    }

    /// Check if this matcher matches a Hyprland window.
    pub fn matches_hyprland_window(&self, title: &str, class: &str) -> bool {
        match self {
            SlotMatcher::HyprTitle(regex) => regex.is_match(title),
            SlotMatcher::HyprClass(regex) => regex.is_match(class),
            SlotMatcher::Cmd(_) => false,
        }
    }
}

/// Parse a list of slot match patterns from config strings.
pub fn parse_slot_matchers(patterns: &[String]) -> Vec<SlotMatcher> {
    patterns
        .iter()
        .filter_map(|s| match SlotMatcher::parse(s) {
            Ok(matcher) => Some(matcher),
            Err(e) => {
                tracing::warn!(pattern = s, error = %e, "Failed to parse slot matcher");
                None
            }
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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

    /// List of patterns to match windows for auto-assignment to slots.
    /// Supports prefix notation:
    /// - "cmd:name" or "name" - match foreground process command
    /// - "hypr-title:regex" - match Hyprland window title
    /// - "hypr-class:regex" - match Hyprland window class
    #[serde(default, alias = "slot-matches")]
    pub slot_matches: Vec<String>,

    /// Font name for slot ID background images.
    /// Uses fontconfig pattern syntax (e.g., "DejaVu Sans Mono:bold").
    /// Defaults to "monospace:bold" if not specified.
    #[serde(default = "default_slot_id_font", alias = "slot-id-font")]
    pub slot_id_font: String,
}

impl Default for KittyConfig {
    fn default() -> Self {
        Self {
            background_color_cmd: None,
            base_background_color: default_kitty_base_background(),
            output_command: None,
            slot_matches: Vec::new(),
            slot_id_font: default_slot_id_font(),
        }
    }
}

fn default_kitty_base_background() -> String {
    "#1e1e2e".to_string()
}

fn default_slot_id_font() -> String {
    "monospace:bold".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClientConfig {
    #[serde(default)]
    pub source: Option<String>,

    #[serde(default, alias = "multi-slot-handler")]
    pub multi_slot_handler: Option<crate::daemon::MultiSlotHandler>,
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

    #[serde(default = "default_chunk_size", alias = "chunk-size")]
    pub chunk_size: usize,

    #[serde(
        default = "default_periodic_transcription_interval",
        alias = "periodic-transcription-interval"
    )]
    pub periodic_transcription_interval: f32,

    #[serde(
        default = "default_transcription_window",
        alias = "transcription-window"
    )]
    pub transcription_window: f32,

    #[serde(default = "default_transcription_lag", alias = "transcription-lag")]
    pub transcription_lag: f32,

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
            chunk_size: default_chunk_size(),
            periodic_transcription_interval: default_periodic_transcription_interval(),
            transcription_window: default_transcription_window(),
            transcription_lag: default_transcription_lag(),
            output_command: None,
            socket_name: default_socket_name(),
        }
    }
}

fn default_model() -> String {
    "base".to_string()
}

fn default_language() -> String {
    "en".to_string()
}

fn default_silence_duration() -> f32 {
    2.0
}

fn default_silence_threshold() -> f32 {
    0.002
}

fn default_min_speech_duration() -> f32 {
    0.5
}

fn default_chunk_size() -> usize {
    1024
}

fn default_periodic_transcription_interval() -> f32 {
    1.0
}

fn default_transcription_window() -> f32 {
    5.0
}

fn default_transcription_lag() -> f32 {
    1.0
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

impl ClientConfig {
    /// Load client configuration from XDG config directory
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
    pub fn load_from_file<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config_file: ConfigFile = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        tracing::info!("Loaded config from: {}", path.display());
        Ok(config_file.daemon)
    }

    /// Get the socket path in XDG runtime directory
    pub fn socket_path(&self) -> Result<PathBuf> {
        let xdg_dirs = BaseDirectories::new();

        let runtime_dir = xdg_dirs
            .get_runtime_directory()
            .context("No XDG_RUNTIME_DIR available")?;

        Ok(runtime_dir.join(&self.socket_name))
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
        assert_eq!(config.silence_duration, 2.0);
        assert_eq!(config.silence_threshold, 0.002);
        assert_eq!(config.min_speech_duration, 0.5);
        assert_eq!(config.chunk_size, 1024);
        assert_eq!(config.periodic_transcription_interval, 1.0);
        assert_eq!(config.transcription_window, 5.0);
        assert_eq!(config.transcription_lag, 1.0);
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
        assert_eq!(config.silence_duration, 2.0); // default
        assert_eq!(config.chunk_size, 1024); // default
        assert_eq!(config.periodic_transcription_interval, 1.0); // default
        assert_eq!(config.transcription_window, 5.0); // default
        assert_eq!(config.transcription_lag, 1.0); // default
        assert_eq!(
            config.output_command,
            Some("tmux load-buffer -".to_string())
        );
    }

    #[test]
    fn test_config_with_aliases() {
        let toml_str = r#"
            model = "base"
            silence-duration = 2.0
            silence-threshold = 0.02
            min-speech-duration = 1.0
        "#;

        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.model, "base");
        assert_eq!(config.silence_duration, 2.0);
        assert_eq!(config.silence_threshold, 0.02);
        assert_eq!(config.min_speech_duration, 1.0);
    }

    #[test]
    fn test_config_file_default() {
        let config_file = ConfigFile::default();
        assert_eq!(config_file.daemon.model, "base");
        assert_eq!(config_file.daemon.language, "en");
        assert_eq!(config_file.daemon.silence_duration, 2.0);
        assert_eq!(config_file.daemon.chunk_size, 1024);
        assert_eq!(config_file.daemon.periodic_transcription_interval, 1.0);
        assert_eq!(config_file.daemon.transcription_window, 5.0);
        assert_eq!(config_file.daemon.transcription_lag, 1.0);
        assert!(config_file.client.source.is_none());
        assert_eq!(config_file.kitty.base_background_color, "#1e1e2e");
    }

    #[test]
    fn test_config_file_serialization() {
        let config_file = ConfigFile::default();
        let toml_str = toml::to_string(&config_file).unwrap();
        let parsed: ConfigFile = toml::from_str(&toml_str).unwrap();

        assert_eq!(config_file.daemon.model, parsed.daemon.model);
        assert_eq!(
            config_file.kitty.base_background_color,
            parsed.kitty.base_background_color
        );
    }

    #[test]
    fn test_client_config_default() {
        let client_config = ClientConfig::default();
        assert!(client_config.source.is_none());
        assert!(client_config.multi_slot_handler.is_none());
    }

    #[test]
    fn test_client_config_with_source() {
        let toml_str = r#"
            source = "alsa_input.usb-microphone"
        "#;

        let client_config: ClientConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            client_config.source,
            Some("alsa_input.usb-microphone".to_string())
        );
    }

    #[test]
    fn test_kitty_config_default() {
        let kitty_config = KittyConfig::default();
        assert!(kitty_config.background_color_cmd.is_none());
        assert_eq!(kitty_config.base_background_color, "#1e1e2e");
        assert!(kitty_config.output_command.is_none());
    }

    #[test]
    fn test_kitty_config_with_values() {
        let toml_str = r##"
            background-color-cmd = "kitty @ set-colors --match id:{window_id} background={color}"
            base-background-color = "#282828"
            output-command = "kitty @ send-text --match id:{window_id}"
        "##;

        let kitty_config: KittyConfig = toml::from_str(toml_str).unwrap();
        assert!(kitty_config.background_color_cmd.is_some());
        assert_eq!(kitty_config.base_background_color, "#282828");
        assert!(kitty_config.output_command.is_some());
    }

    #[test]
    fn test_full_config_file_parsing() {
        let toml_str = r##"
            [daemon]
            model = "base"
            silence-duration = 2.5

            [client]
            source = "my-microphone"

            [kitty]
            base-background-color = "#202020"
        "##;

        let config_file: ConfigFile = toml::from_str(toml_str).unwrap();
        assert_eq!(config_file.daemon.model, "base");
        assert_eq!(config_file.daemon.silence_duration, 2.5);
        assert_eq!(config_file.client.source, Some("my-microphone".to_string()));
        assert_eq!(config_file.kitty.base_background_color, "#202020");
    }

    #[test]
    fn test_slot_matcher_parse_cmd_prefix() {
        let matcher = SlotMatcher::parse("cmd:claude").unwrap();
        assert!(matches!(matcher, SlotMatcher::Cmd(ref s) if s == "claude"));
        assert!(matcher.matches_cmd("claude"));
        assert!(!matcher.matches_cmd("nvim"));
    }

    #[test]
    fn test_slot_matcher_parse_bare_string() {
        let matcher = SlotMatcher::parse("claude").unwrap();
        assert!(matches!(matcher, SlotMatcher::Cmd(ref s) if s == "claude"));
        assert!(matcher.matches_cmd("claude"));
    }

    #[test]
    fn test_slot_matcher_parse_hypr_title() {
        let matcher = SlotMatcher::parse("hypr-title:MultiMessage:.*").unwrap();
        assert!(matches!(matcher, SlotMatcher::HyprTitle(_)));
        assert!(matcher.is_hyprland_matcher());
        assert!(matcher.matches_hyprland_window("MultiMessage: test", "kitty"));
        assert!(!matcher.matches_hyprland_window("Other title", "kitty"));
    }

    #[test]
    fn test_slot_matcher_parse_hypr_class() {
        let matcher = SlotMatcher::parse("hypr-class:^kitty$").unwrap();
        assert!(matches!(matcher, SlotMatcher::HyprClass(_)));
        assert!(matcher.is_hyprland_matcher());
        assert!(matcher.matches_hyprland_window("any title", "kitty"));
        assert!(!matcher.matches_hyprland_window("any title", "alacritty"));
    }

    #[test]
    fn test_slot_matcher_invalid_regex() {
        let result = SlotMatcher::parse("hypr-title:[invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_slot_matchers_mixed() {
        let patterns = vec![
            "claude".to_string(),
            "cmd:nvim".to_string(),
            "hypr-title:Test.*".to_string(),
        ];
        let matchers = parse_slot_matchers(&patterns);
        assert_eq!(matchers.len(), 3);
    }

    #[test]
    fn test_parse_slot_matchers_skips_invalid() {
        let patterns = vec![
            "claude".to_string(),
            "hypr-title:[invalid".to_string(),
            "nvim".to_string(),
        ];
        let matchers = parse_slot_matchers(&patterns);
        assert_eq!(matchers.len(), 2);
    }

    #[test]
    fn test_kitty_config_slot_matches() {
        let toml_str = r##"
            slot-matches = ["cmd:claude", "hypr-title:Test.*"]
        "##;

        let kitty_config: KittyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(kitty_config.slot_matches.len(), 2);
        assert_eq!(kitty_config.slot_matches[0], "cmd:claude");
        assert_eq!(kitty_config.slot_matches[1], "hypr-title:Test.*");
    }

}
