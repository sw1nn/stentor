use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::io::{Read, Write};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[derive(Serialize)]
struct KittyCommand<T = SetColorsPayload> {
    cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<T>,
}

#[derive(Serialize)]
struct SetColorsPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    colors: Option<HashMap<String, u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    match_window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reset: Option<bool>,
}

#[derive(Serialize)]
struct SetUserVarsPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    match_window: Option<String>,
    #[serde(flatten)]
    vars: HashMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeWindow {
    pub id: u64,
    pub cwd: String,
    pub slot_num: usize,
}

fn get_kitty_socket_name() -> Result<String> {
    if let Ok(listen_on) = env::var("KITTY_LISTEN_ON") {
        if let Some(path) = listen_on.strip_prefix("unix:@") {
            return Ok(path.to_string());
        } else if let Some(path) = listen_on.strip_prefix("unix:") {
            return Ok(path.to_string());
        }
    }
    Ok("kitty".to_string())
}

fn send_kitty_command<T: Serialize>(cmd: KittyCommand<T>) -> Result<()> {
    let json = serde_json::to_string(&cmd)?;
    tracing::debug!("Sending command JSON: {}", json);
    let socket_name = get_kitty_socket_name()?;

    let addr = std::os::unix::net::SocketAddr::from_abstract_name(socket_name.as_bytes())
        .context("Failed to create abstract socket address")?;
    let mut stream =
        UnixStream::connect_addr(&addr).context("Failed to connect to Kitty socket")?;
    tracing::debug!("Connected to socket: @{}", socket_name);

    // Use DCS escape sequence format: \x1bP@kitty-cmd<JSON>\x1b\\
    let encoded = format!("\x1bP@kitty-cmd{}\x1b\\", json);
    stream.write_all(encoded.as_bytes())?;
    stream.flush()?;

    // Set read timeout
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    // Read response
    let mut resp = Vec::new();
    let mut buffer = [0u8; 4096];

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                tracing::trace!("Read {} bytes", n);
                resp.extend_from_slice(&buffer[..n]);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                tracing::trace!("Read timeout - assuming no more data");
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    // Parse and check response
    let response_str = String::from_utf8_lossy(&resp);
    tracing::trace!("Received response: {:?}", response_str);
    if let Some(start) = response_str.find("@kitty-cmd") {
        let json_start = start + 10;
        if let Some(end) = response_str[json_start..].find("\x1b\\") {
            let json_str = &response_str[json_start..json_start + end];
            tracing::trace!("Response JSON: {}", json_str);
            let response: Value =
                serde_json::from_str(json_str).context("Failed to parse Kitty response")?;

            if !response
                .get("ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                anyhow::bail!("Kitty command failed: {}", json_str);
            }
        }
    }

    Ok(())
}

pub fn list_kitty_windows() -> Result<Value> {
    let socket_name = get_kitty_socket_name()?;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(socket_name.as_bytes())
        .context("Failed to create abstract socket address")?;
    let mut stream =
        UnixStream::connect_addr(&addr).context("Failed to connect to Kitty socket")?;
    tracing::debug!("Connected to socket for listing windows: @{}", socket_name);

    let cmd = r#"{"cmd":"ls","version":[0,37,2]}"#;
    let encoded = format!("\x1bP@kitty-cmd{}\x1b\\", cmd);

    stream.write_all(encoded.as_bytes())?;
    stream.flush()?;
    tracing::debug!("Sent ls command");

    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    let mut resp = Vec::new();
    let mut buffer = [0u8; 4096];

    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                tracing::trace!("Read {} bytes", n);
                resp.extend_from_slice(&buffer[..n]);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                tracing::trace!("Read timeout - assuming no more data");
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    tracing::trace!("Total received {} bytes", resp.len());
    let response_str = String::from_utf8_lossy(&resp);

    if let Some(start) = response_str.find("@kitty-cmd") {
        let json_start = start + 10;
        if let Some(end) = response_str[json_start..].find("\x1b\\") {
            let json_str = &response_str[json_start..json_start + end];
            tracing::trace!("Response JSON length: {} bytes", json_str.len());

            let response: Value =
                serde_json::from_str(json_str).context("Failed to parse Kitty response")?;

            if response
                .get("ok")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                if let Some(data) = response.get("data") {
                    if let Some(data_str) = data.as_str() {
                        let parsed_data: Value =
                            serde_json::from_str(data_str).context("Failed to parse Kitty data")?;
                        return Ok(parsed_data);
                    } else {
                        return Ok(data.clone());
                    }
                }
            } else {
                anyhow::bail!("Kitty ls command returned ok=false");
            }
        }
    }

    anyhow::bail!("Failed to parse Kitty response")
}

pub fn find_stentor_windows(data: &Value) -> Vec<ClaudeWindow> {
    let mut stentor_windows: Vec<(usize, ClaudeWindow)> = Vec::new();

    if let Some(os_windows) = data.as_array() {
        tracing::debug!("Scanning {} OS window(s)", os_windows.len());
        for os_window in os_windows {
            if let Some(tabs) = os_window.get("tabs").and_then(|t| t.as_array()) {
                for tab in tabs {
                    if let Some(windows) = tab.get("windows").and_then(|w| w.as_array()) {
                        for window in windows {
                            // Check if window has STENTOR_SLOT env var and extract its value
                            let slot_number =
                                if let Some(env) = window.get("env").and_then(|e| e.as_object()) {
                                    if let Some(slot_str) =
                                        env.get("STENTOR_SLOT").and_then(|v| v.as_str())
                                    {
                                        slot_str.parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                };

                            if slot_number.is_none() {
                                continue;
                            }

                            let slot_num = slot_number.unwrap();

                            // Window has STENTOR_SLOT - include it
                            if let Some(id) = window.get("id").and_then(|i| i.as_u64()) {
                                let cwd = window
                                    .get("cwd")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                tracing::debug!(
                                    "Found STENTOR_SLOT window: id={}, cwd={}, slot={}",
                                    id,
                                    cwd,
                                    slot_num
                                );
                                stentor_windows
                                    .push((slot_num, ClaudeWindow { id, cwd, slot_num }));
                            }
                        }
                    }
                }
            }
        }
    }

    tracing::debug!("Total STENTOR windows found: {}", stentor_windows.len());

    // Return ClaudeWindow structs in discovery order (not sorted)
    // This ensures Alt+1, Alt+2, Alt+3, etc. map to the 1st, 2nd, 3rd discovered windows
    stentor_windows
        .into_iter()
        .map(|(_, window)| window)
        .collect()
}

/// Find the next available STENTOR_SLOT (1-8)
/// Returns None if all slots are occupied
pub fn find_available_slot() -> Result<Option<usize>> {
    let data = list_kitty_windows()?;

    // Extract slot numbers from existing windows
    let mut occupied_slots = std::collections::HashSet::new();

    if let Some(os_windows) = data.as_array() {
        for os_window in os_windows {
            if let Some(tabs) = os_window.get("tabs").and_then(|t| t.as_array()) {
                for tab in tabs {
                    if let Some(windows) = tab.get("windows").and_then(|w| w.as_array()) {
                        for window in windows {
                            if let Some(env) = window.get("env").and_then(|e| e.as_object())
                                && let Some(slot_str) =
                                    env.get("STENTOR_SLOT").and_then(|v| v.as_str())
                                && let Ok(slot_num) = slot_str.parse::<usize>()
                                && (1..=8).contains(&slot_num)
                            {
                                occupied_slots.insert(slot_num);
                            }
                        }
                    }
                }
            }
        }
    }

    // Find first available slot
    for slot in 1..=8 {
        if !occupied_slots.contains(&slot) {
            tracing::info!("Found available slot: {}", slot);
            return Ok(Some(slot));
        }
    }

    tracing::warn!("No available slots (all 8 slots occupied)");
    Ok(None)
}

/// Launch a new Kitty window with the given command and STENTOR_SLOT env var
/// If launched from within a Kitty window, closes the current window after successful launch
pub fn launch_with_slot(slot: usize, command: &[String]) -> Result<()> {
    if !(1..=8).contains(&slot) {
        anyhow::bail!("Slot must be between 1 and 8, got {}", slot);
    }

    // Get current window ID for closing later
    let current_window_id = env::var("KITTY_WINDOW_ID").ok();

    // Build the kitty @ launch command
    let mut cmd = std::process::Command::new("kitty");
    cmd.arg("@")
        .arg("launch")
        .arg("--cwd=current")
        .arg("--env")
        .arg(format!("STENTOR_SLOT={}", slot));

    // Add user command
    for arg in command {
        cmd.arg(arg);
    }

    tracing::info!(
        "Launching kitty window in slot {} with command: {:?}",
        slot,
        command
    );

    let output = cmd
        .output()
        .context("Failed to execute kitty @ launch command")?;

    if !output.status.success() {
        anyhow::bail!(
            "kitty @ launch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tracing::info!("Successfully launched window in slot {}", slot);

    // Close the current window if we're running inside Kitty
    if let Some(window_id) = current_window_id {
        tracing::info!("Closing current window {}", window_id);
        let close_result = std::process::Command::new("kitty")
            .arg("@")
            .arg("close-window")
            .arg("--match")
            .arg(format!("id:{}", window_id))
            .output();

        if let Err(e) = close_result {
            tracing::warn!("Failed to close current window: {}", e);
        }
    }

    Ok(())
}

fn hex_to_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.trim_start_matches('#');
    let color_str = if hex.len() == 8 {
        &hex[0..6] // Ignore alpha channel
    } else {
        hex
    };

    if color_str.len() != 6 {
        return None;
    }

    let r = u8::from_str_radix(&color_str[0..2], 16).ok()?;
    let g = u8::from_str_radix(&color_str[2..4], 16).ok()?;
    let b = u8::from_str_radix(&color_str[4..6], 16).ok()?;

    Some((r, g, b))
}

fn get_contrast_color(hex: &str) -> (u8, u8, u8) {
    let (r, g, b) = hex_to_rgb(hex).unwrap_or((128, 128, 128));

    // Simple perceived brightness calculation
    let brightness = (r as f32 * 0.299 + g as f32 * 0.587 + b as f32 * 0.114) / 255.0;

    if brightness > 0.6 {
        (0, 0, 0) // Black text for light backgrounds
    } else {
        (255, 255, 255) // White text for dark/medium backgrounds
    }
}

fn rgb_to_int(r: u8, g: u8, b: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

pub fn set_background_color(color: &str, window_id: u64) -> Result<()> {
    let match_window = Some(format!("id:{}", window_id));

    let payload = if color.is_empty() {
        // Empty string means reset to startup values
        SetColorsPayload {
            colors: None,
            match_window,
            reset: Some(true),
        }
    } else {
        let (bg_r, bg_g, bg_b) = hex_to_rgb(color).unwrap_or((128, 128, 128));
        let (fg_r, fg_g, fg_b) = get_contrast_color(color);

        let mut colors = HashMap::new();
        colors.insert("background".to_string(), rgb_to_int(bg_r, bg_g, bg_b));
        colors.insert("foreground".to_string(), rgb_to_int(fg_r, fg_g, fg_b));

        SetColorsPayload {
            colors: Some(colors),
            match_window,
            reset: None,
        }
    };

    let cmd = KittyCommand {
        cmd: "set-colors".to_string(),
        version: Some(vec![0, 26, 0]),
        payload: Some(payload),
    };

    send_kitty_command(cmd)
}

/// Set both background color and environment variable in a single socket connection
/// This is much faster than calling set_background_color and set_window_env_var separately
/// Fire-and-forget style: sends both commands without waiting for responses
pub fn set_window_color_and_env(
    color: &str,
    env_var_name: &str,
    env_var_value: &str,
    window_id: u64,
) -> Result<()> {
    let socket_name = get_kitty_socket_name()?;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(socket_name.as_bytes())
        .context("Failed to create abstract socket address")?;
    let mut stream =
        UnixStream::connect_addr(&addr).context("Failed to connect to Kitty socket")?;

    let match_window = Some(format!("id:{}", window_id));

    // Build set-colors command
    let (bg_r, bg_g, bg_b) = hex_to_rgb(color).unwrap_or((128, 128, 128));
    let (fg_r, fg_g, fg_b) = get_contrast_color(color);

    let mut colors = HashMap::new();
    colors.insert("background".to_string(), rgb_to_int(bg_r, bg_g, bg_b));
    colors.insert("foreground".to_string(), rgb_to_int(fg_r, fg_g, fg_b));

    let color_payload = SetColorsPayload {
        colors: Some(colors),
        match_window: match_window.clone(),
        reset: None,
    };

    let color_cmd = KittyCommand {
        cmd: "set-colors".to_string(),
        version: Some(vec![0, 26, 0]),
        payload: Some(color_payload),
    };

    // Build set-user-vars command
    let mut vars = HashMap::new();
    vars.insert(env_var_name.to_string(), env_var_value.to_string());

    let env_payload = SetUserVarsPayload { match_window, vars };

    let env_cmd = KittyCommand {
        cmd: "set-user-vars".to_string(),
        version: Some(vec![0, 35, 0]),
        payload: Some(env_payload),
    };

    // Serialize both commands
    let color_json = serde_json::to_string(&color_cmd)?;
    let env_json = serde_json::to_string(&env_cmd)?;

    tracing::debug!(
        "Sending batched commands for window {} (fire-and-forget)",
        window_id
    );

    // Send both commands without waiting for responses (fire-and-forget)
    let color_encoded = format!("\x1bP@kitty-cmd{}\x1b\\", color_json);
    stream.write_all(color_encoded.as_bytes())?;

    let env_encoded = format!("\x1bP@kitty-cmd{}\x1b\\", env_json);
    stream.write_all(env_encoded.as_bytes())?;

    stream.flush()?;

    tracing::debug!("Batched commands sent for window {}", window_id);
    Ok(())
}

use crate::config::KittyConfig;
use crate::dialog::DestinationSlot;
use crate::multi_slot::{HandlerUIMessage, MultiSlotHandler};
use crate::palette::Palette;
use async_channel::Sender;
use std::thread;

/// Get a label for a directory, using git branch name if available
/// If in a git repository, returns "repo\nbranch_name"
/// Repo name is extracted from the remote origin URL
/// Otherwise returns the last directory component
fn parse_worktree_label(cwd: &str) -> String {
    // Try to get the git branch name
    let branch_output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("branch")
        .arg("--show-current")
        .output();

    if let Ok(output) = branch_output
        && output.status.success()
    {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !branch.is_empty() {
            // Get the remote origin URL
            let origin_output = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .arg("remote")
                .arg("get-url")
                .arg("origin")
                .output();

            if let Ok(origin) = origin_output
                && origin.status.success()
            {
                let origin_url = String::from_utf8_lossy(&origin.stdout).trim().to_string();
                // Extract repo name from URL (last segment, without .git)
                if let Some(repo_name) = origin_url.split('/').next_back() {
                    let repo_name = repo_name.trim_end_matches(".git");
                    // Return multi-line label: repo name on first line, branch on second
                    return format!("{}\n{}", repo_name, branch);
                }
            }
        }
    }

    // Fall back to last directory component
    cwd.split('/').next_back().unwrap_or(cwd).to_string()
}

/// Kitty terminal multi-slot handler implementation
pub struct KittyMultiSlotHandler {
    config: KittyConfig,
}

impl KittyMultiSlotHandler {
    pub fn new(config: KittyConfig) -> Self {
        Self { config }
    }

    fn get_background_color(&self) -> String {
        // If a custom command is configured, try to use it
        if let Some(cmd) = &self.config.background_color_cmd {
            tracing::info!("Retrieving background color using command: {}", cmd);

            match std::process::Command::new("sh").arg("-c").arg(cmd).output() {
                Ok(output) => {
                    if output.status.success() {
                        let color = String::from_utf8_lossy(&output.stdout).trim().to_string();
                        if !color.is_empty() {
                            tracing::info!("Retrieved background color: {}", color);
                            return color;
                        } else {
                            tracing::warn!("Command returned empty output, using default");
                        }
                    } else {
                        tracing::warn!(
                            "Command failed with status {:?}: {}",
                            output.status,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to execute background color retrieval command: {}",
                        e
                    );
                }
            }
        }

        // Fall back to configured base background color
        tracing::info!(
            "Using configured background color: {}",
            self.config.base_background_color
        );
        self.config.base_background_color.clone()
    }
}

impl MultiSlotHandler for KittyMultiSlotHandler {
    fn setup(&self, ui_tx: Sender<HandlerUIMessage>) -> Result<()> {
        // Immediately show inactive slots so Alt+1-8 labels are visible
        let mut inactive_destinations = Vec::new();
        for slot_num in 1..=8 {
            inactive_destinations.push(DestinationSlot::inactive(slot_num));
        }
        let _ = ui_tx.send_blocking(HandlerUIMessage::SetDestinations(inactive_destinations));
        tracing::info!("Initialized 8 inactive destination slots");

        // Clone ui_tx for thread
        let ui_tx_for_kitty = ui_tx.clone();
        let base_bg = self.config.base_background_color.clone();

        thread::spawn(move || {
            tracing::info!("Background: Starting Kitty window discovery");

            match list_kitty_windows() {
                Ok(data) => {
                    let stentor_windows = find_stentor_windows(&data);
                    tracing::info!(
                        "Background: Found {} STENTOR windows",
                        stentor_windows.len()
                    );

                    let palette = Palette::new(&base_bg);
                    let mut destinations = Vec::new();
                    let mut modified_window_ids = Vec::new();

                    // Process each found window and update UI incrementally
                    for (i, window) in stentor_windows.iter().enumerate().take(8) {
                        // Use the actual STENTOR_SLOT value from the window, not the button index
                        let slot_num = window.slot_num;
                        if let Some((_name, color_hex)) = palette.get_slot_color(slot_num) {
                            // Set background color and env var in one batched call
                            let env_var_name = format!("STENTOR_SLOT_{}", slot_num);
                            if let Err(e) =
                                set_window_color_and_env(color_hex, &env_var_name, "1", window.id)
                            {
                                tracing::warn!(
                                    "Failed to set color and env var for window {}: {}",
                                    window.id,
                                    e
                                );
                            } else {
                                tracing::debug!(
                                    "Set color {} and env var {} for window {}",
                                    color_hex,
                                    env_var_name,
                                    window.id
                                );
                                modified_window_ids.push(window.id);
                            }

                            // Create destination slot with label and send immediately
                            // Parse the path to check if it's a worktree
                            let label = parse_worktree_label(&window.cwd);
                            destinations.push(DestinationSlot::with_label(
                                slot_num,
                                label,
                                color_hex.to_string(),
                            ));

                            // Update UI incrementally - send current state after each window is processed
                            let mut current_destinations = destinations.clone();
                            // Add remaining slots as inactive
                            for j in (i + 1)..8 {
                                current_destinations.push(DestinationSlot::inactive(j + 1));
                            }
                            tracing::debug!(
                                "Background: Sending incremental update for slot {}",
                                slot_num
                            );
                            let _ = ui_tx_for_kitty.send_blocking(
                                HandlerUIMessage::SetDestinations(current_destinations),
                            );
                        }
                    }

                    // Store window IDs for cleanup
                    if !modified_window_ids.is_empty() {
                        let _ = ui_tx_for_kitty
                            .send_blocking(HandlerUIMessage::StoreWindowIds(modified_window_ids));
                    }

                    // If no windows were found, the initial inactive slots are already showing
                    tracing::info!(
                        "Background: Finished processing {} STENTOR windows",
                        stentor_windows.len()
                    );
                }
                Err(e) => {
                    tracing::warn!("Background: Failed to list Kitty windows: {}", e);
                    // Empty slots are already showing from initial setup, no need to update
                }
            }
        });

        Ok(())
    }

    fn cleanup(&self, window_ids: Vec<u64>) -> Result<()> {
        if window_ids.is_empty() {
            return Ok(());
        }

        let background_color = self.get_background_color();
        tracing::info!(
            "Resetting background color for {} Kitty windows to {}",
            window_ids.len(),
            background_color
        );

        for window_id in window_ids {
            if let Err(e) = set_background_color(&background_color, window_id) {
                tracing::warn!(
                    "Failed to reset background color for window {}: {}",
                    window_id,
                    e
                );
            } else {
                tracing::debug!("Reset background color for window {}", window_id);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Tests for hex_to_rgb
    #[test]
    fn test_hex_to_rgb_valid() {
        assert_eq!(hex_to_rgb("#ff0000"), Some((255, 0, 0)));
        assert_eq!(hex_to_rgb("#00ff00"), Some((0, 255, 0)));
        assert_eq!(hex_to_rgb("#0000ff"), Some((0, 0, 255)));
        assert_eq!(hex_to_rgb("#ffffff"), Some((255, 255, 255)));
        assert_eq!(hex_to_rgb("#000000"), Some((0, 0, 0)));
    }

    #[test]
    fn test_hex_to_rgb_without_hash() {
        assert_eq!(hex_to_rgb("ff0000"), Some((255, 0, 0)));
        assert_eq!(hex_to_rgb("00ff00"), Some((0, 255, 0)));
    }

    #[test]
    fn test_hex_to_rgb_with_alpha() {
        // Should ignore alpha channel
        assert_eq!(hex_to_rgb("#ff0000ff"), Some((255, 0, 0)));
        assert_eq!(hex_to_rgb("#00ff0080"), Some((0, 255, 0)));
    }

    #[test]
    fn test_hex_to_rgb_invalid() {
        assert_eq!(hex_to_rgb("#ff00"), None); // Too short
        assert_eq!(hex_to_rgb("#ff00000"), None); // 7 chars (invalid)
        assert_eq!(hex_to_rgb("#gggggg"), None); // Invalid hex
        assert_eq!(hex_to_rgb(""), None); // Empty
    }

    #[test]
    fn test_hex_to_rgb_mixed_case() {
        assert_eq!(hex_to_rgb("#FF0000"), Some((255, 0, 0)));
        assert_eq!(hex_to_rgb("#AbCdEf"), Some((171, 205, 239)));
    }

    // Tests for get_contrast_color
    #[test]
    fn test_get_contrast_color_light_background() {
        // Light backgrounds should get black text
        let (r, g, b) = get_contrast_color("#ffffff");
        assert_eq!((r, g, b), (0, 0, 0));

        let (r, g, b) = get_contrast_color("#f0f0f0");
        assert_eq!((r, g, b), (0, 0, 0));
    }

    #[test]
    fn test_get_contrast_color_dark_background() {
        // Dark backgrounds should get white text
        let (r, g, b) = get_contrast_color("#000000");
        assert_eq!((r, g, b), (255, 255, 255));

        let (r, g, b) = get_contrast_color("#1e1e2e");
        assert_eq!((r, g, b), (255, 255, 255));
    }

    #[test]
    fn test_get_contrast_color_invalid_hex() {
        // Invalid hex falls back to (128, 128, 128) which has brightness ~0.5 -> white text
        let (r, g, b) = get_contrast_color("invalid");
        assert_eq!((r, g, b), (255, 255, 255));
    }

    // Tests for rgb_to_int
    #[test]
    fn test_rgb_to_int() {
        assert_eq!(rgb_to_int(255, 0, 0), 0xff0000);
        assert_eq!(rgb_to_int(0, 255, 0), 0x00ff00);
        assert_eq!(rgb_to_int(0, 0, 255), 0x0000ff);
        assert_eq!(rgb_to_int(255, 255, 255), 0xffffff);
        assert_eq!(rgb_to_int(0, 0, 0), 0x000000);
        assert_eq!(rgb_to_int(171, 205, 239), 0xabcdef);
    }

    // Tests for get_kitty_socket_name
    #[test]
    fn test_get_kitty_socket_name_with_env() {
        unsafe {
            std::env::set_var("KITTY_LISTEN_ON", "unix:/tmp/test.sock");
        }
        let result = get_kitty_socket_name();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "/tmp/test.sock");
        unsafe {
            std::env::remove_var("KITTY_LISTEN_ON");
        }
    }

    #[test]
    fn test_get_kitty_socket_name_without_prefix() {
        unsafe {
            std::env::set_var("KITTY_LISTEN_ON", "/tmp/test.sock");
        }
        let result = get_kitty_socket_name();
        assert!(result.is_ok());
        // Without "unix:" prefix, it falls back to default "kitty"
        assert_eq!(result.unwrap(), "kitty");
        unsafe {
            std::env::remove_var("KITTY_LISTEN_ON");
        }
    }

    #[test]
    fn test_get_kitty_socket_name_missing_env() {
        // Don't remove the env var - just check the default behavior
        // when KITTY_LISTEN_ON is not set or doesn't match expected patterns
        let result = get_kitty_socket_name();
        // Should return default "kitty" when env var is not properly formatted
        assert!(result.is_ok());
    }

    // Tests for find_stentor_windows
    #[test]
    fn test_find_stentor_windows_empty() {
        let data = json!([]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 0);
    }

    #[test]
    fn test_find_stentor_windows_no_stentor() {
        let data = json!([
            {
                "tabs": [{
                    "windows": [{
                        "id": 1,
                        "cwd": "/home/test",
                        "env": {}
                    }]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 0);
    }

    #[test]
    fn test_find_stentor_windows_with_stentor() {
        let data = json!([
            {
                "tabs": [{
                    "windows": [{
                        "id": 1,
                        "cwd": "/home/test",
                        "env": {
                            "STENTOR_SLOT": "1"
                        }
                    }]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 1);
        assert_eq!(windows[0].slot_num, 1);
    }

    #[test]
    fn test_find_stentor_windows_with_slot() {
        let data = json!([
            {
                "tabs": [{
                    "windows": [{
                        "id": 1,
                        "cwd": "/home/test",
                        "env": {
                            "STENTOR_SLOT": "3"
                        }
                    }]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, 1);
        assert_eq!(windows[0].slot_num, 3);
    }

    #[test]
    fn test_find_stentor_windows_multiple() {
        let data = json!([
            {
                "tabs": [{
                    "windows": [
                        {
                            "id": 1,
                            "cwd": "/home/first",
                            "env": {
                                "STENTOR_SLOT": "1"
                            }
                        },
                        {
                            "id": 2,
                            "cwd": "/home/normal",
                            "env": {}
                        },
                        {
                            "id": 3,
                            "cwd": "/home/second",
                            "env": {
                                "STENTOR_SLOT": "2"
                            }
                        }
                    ]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, 1);
        assert_eq!(windows[0].slot_num, 1);
        assert_eq!(windows[1].id, 3);
        assert_eq!(windows[1].slot_num, 2);
    }

    #[test]
    fn test_find_stentor_windows_invalid_json_structure() {
        // Missing required fields should be skipped gracefully
        let data = json!([
            {
                "tabs": [{
                    "windows": [{
                        "cwd": "/home/test",
                        "env": {
                            "STENTOR_SLOT": "1"
                        }
                        // missing id
                    }]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 0);
    }

    #[test]
    fn test_slot_range_validation() {
        // Test that only slots 1-8 are valid
        for slot in 1..=8 {
            assert!((1..=8).contains(&slot), "Slot {} should be valid", slot);
        }
        assert!(!(1..=8).contains(&0));
        assert!(!(1..=8).contains(&9));
        assert!(!(1..=8).contains(&100));
    }

    #[test]
    fn test_find_stentor_windows_various_slot_numbers() {
        // Test that any numeric STENTOR_SLOT value is accepted
        let data = json!([
            {
                "tabs": [{
                    "windows": [
                        {
                            "id": 1,
                            "cwd": "/home/test",
                            "env": {"STENTOR_SLOT": "0"}
                        },
                        {
                            "id": 2,
                            "cwd": "/home/test",
                            "env": {"STENTOR_SLOT": "9"}
                        },
                        {
                            "id": 3,
                            "cwd": "/home/test",
                            "env": {"STENTOR_SLOT": "5"}
                        }
                    ]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        // All numeric slots are accepted
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].slot_num, 0);
        assert_eq!(windows[1].slot_num, 9);
        assert_eq!(windows[2].slot_num, 5);
    }

    #[test]
    fn test_find_stentor_windows_non_numeric_slot() {
        // Test that non-numeric STENTOR_SLOT values are skipped
        let data = json!([
            {
                "tabs": [{
                    "windows": [
                        {
                            "id": 1,
                            "cwd": "/home/test",
                            "env": {"STENTOR_SLOT": "invalid"}
                        },
                        {
                            "id": 2,
                            "cwd": "/home/test",
                            "env": {"STENTOR_SLOT": ""}
                        }
                    ]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 0);
    }

    #[test]
    fn test_find_stentor_windows_cwd_extraction() {
        // Test that cwd is correctly extracted
        let data = json!([
            {
                "tabs": [{
                    "windows": [{
                        "id": 1,
                        "cwd": "/home/user/project",
                        "env": {"STENTOR_SLOT": "1"}
                    }]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].cwd, "/home/user/project");
    }

    #[test]
    fn test_find_stentor_windows_missing_cwd() {
        // Test fallback to "unknown" when cwd is missing
        let data = json!([
            {
                "tabs": [{
                    "windows": [{
                        "id": 1,
                        "env": {"STENTOR_SLOT": "1"}
                        // no cwd field
                    }]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].cwd, "unknown");
    }

    #[test]
    fn test_find_stentor_windows_multiple_tabs() {
        // Test scanning across multiple tabs
        let data = json!([
            {
                "tabs": [
                    {
                        "windows": [{
                            "id": 1,
                            "cwd": "/tab1",
                            "env": {"STENTOR_SLOT": "1"}
                        }]
                    },
                    {
                        "windows": [{
                            "id": 2,
                            "cwd": "/tab2",
                            "env": {"STENTOR_SLOT": "2"}
                        }]
                    }
                ]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, 1);
        assert_eq!(windows[1].id, 2);
    }

    #[test]
    fn test_find_stentor_windows_multiple_os_windows() {
        // Test scanning across multiple OS windows
        let data = json!([
            {
                "tabs": [{
                    "windows": [{
                        "id": 1,
                        "cwd": "/os1",
                        "env": {"STENTOR_SLOT": "1"}
                    }]
                }]
            },
            {
                "tabs": [{
                    "windows": [{
                        "id": 2,
                        "cwd": "/os2",
                        "env": {"STENTOR_SLOT": "2"}
                    }]
                }]
            }
        ]);
        let windows = find_stentor_windows(&data);
        assert_eq!(windows.len(), 2);
    }
}
