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
                tracing::debug!("Read {} bytes", n);
                resp.extend_from_slice(&buffer[..n]);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                tracing::debug!("Read timeout - assuming no more data");
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    // Parse and check response
    let response_str = String::from_utf8_lossy(&resp);
    tracing::debug!("Received response: {:?}", response_str);
    if let Some(start) = response_str.find("@kitty-cmd") {
        let json_start = start + 10;
        if let Some(end) = response_str[json_start..].find("\x1b\\") {
            let json_str = &response_str[json_start..json_start + end];
            tracing::debug!("Response JSON: {}", json_str);
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
                tracing::debug!("Read {} bytes", n);
                resp.extend_from_slice(&buffer[..n]);
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                tracing::debug!("Read timeout - assuming no more data");
                break;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    tracing::debug!("Total received {} bytes", resp.len());
    let response_str = String::from_utf8_lossy(&resp);

    if let Some(start) = response_str.find("@kitty-cmd") {
        let json_start = start + 10;
        if let Some(end) = response_str[json_start..].find("\x1b\\") {
            let json_str = &response_str[json_start..json_start + end];
            tracing::debug!("Response JSON length: {} bytes", json_str.len());

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
    let mut stentor_windows = Vec::new();

    if let Some(os_windows) = data.as_array() {
        tracing::debug!("Scanning {} OS window(s)", os_windows.len());
        for os_window in os_windows {
            if let Some(tabs) = os_window.get("tabs").and_then(|t| t.as_array()) {
                for tab in tabs {
                    if let Some(windows) = tab.get("windows").and_then(|w| w.as_array()) {
                        for window in windows {
                            // Check if window has STENTOR_SLOT env var
                            let has_stentor_slot = if let Some(user_vars) = window.get("env") {
                                user_vars
                                    .as_object()
                                    .map_or(false, |vars| vars.keys().any(|k| k == "STENTOR_SLOT"))
                            } else {
                                false
                            };

                            if !has_stentor_slot {
                                continue;
                            }

                            // Window has STENTOR_SLOT - include it
                            if let Some(id) = window.get("id").and_then(|i| i.as_u64()) {
                                let cwd = window
                                    .get("cwd")
                                    .and_then(|c| c.as_str())
                                    .unwrap_or("unknown")
                                    .to_string();
                                tracing::debug!(
                                    "Found STENTOR_SLOT window: id={}, cwd={}",
                                    id,
                                    cwd
                                );
                                stentor_windows.push(ClaudeWindow { id, cwd });
                            }
                        }
                    }
                }
            }
        }
    }

    tracing::debug!("Total STENTOR windows found: {}", stentor_windows.len());
    stentor_windows
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

#[allow(dead_code)]
pub fn set_window_env_var(var_name: &str, var_value: &str, window_id: u64) -> Result<()> {
    let match_window = Some(format!("id:{}", window_id));

    let mut vars = HashMap::new();
    vars.insert(var_name.to_string(), var_value.to_string());

    let payload = SetUserVarsPayload { match_window, vars };

    let cmd = KittyCommand {
        cmd: "set-user-vars".to_string(),
        version: Some(vec![0, 35, 0]),
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
