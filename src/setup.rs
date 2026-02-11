//! Interactive setup command for configuring AI coding tools to use AGCP proxy.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use dialoguer::{MultiSelect, theme::ColorfulTheme};

use crate::colors::*;
use crate::config::Config;

/// Regex for matching model_provider setting in codex config
static MODEL_PROVIDER_REGEX: LazyLock<regex_lite::Regex> =
    LazyLock::new(|| regex_lite::Regex::new(r#"model_provider = "[^"]*""#).unwrap());

/// Get the XDG config directory (~/.config), respecting $XDG_CONFIG_HOME.
/// Unlike `dirs::config_dir()` which returns ~/Library/Application Support on macOS,
/// many CLI tools (OpenCode, Crush) follow XDG conventions on all platforms.
fn xdg_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
    }
}

/// Tool configuration definition
struct Tool {
    name: &'static str,
    config_path: PathBuf,
    backup_name: &'static str,
    /// Check if tool is detected (config exists or can be created)
    detect: fn(&Path) -> bool,
    /// Check if already configured for AGCP
    is_configured: fn(&Path, &str) -> bool,
    /// Apply AGCP configuration
    configure: fn(&Path, &str) -> Result<(), String>,
}

/// Get the AGCP proxy URL based on the running daemon's address, falling back to config
fn get_proxy_url() -> String {
    let (host, port) = crate::config::get_daemon_host_port();
    format!("http://{}:{}", host, port)
}

/// Get the backups directory
fn get_backups_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("agcp")
        .join("backups")
}

/// Backup a config file
fn backup_config(config_path: &Path, backup_name: &str) -> Result<(), String> {
    if !config_path.exists() {
        return Ok(()); // Nothing to backup
    }

    let backups_dir = get_backups_dir();
    fs::create_dir_all(&backups_dir).map_err(|e| format!("Failed to create backups dir: {}", e))?;

    let backup_path = backups_dir.join(backup_name);
    fs::copy(config_path, &backup_path)
        .map_err(|e| format!("Failed to backup {}: {}", config_path.display(), e))?;

    Ok(())
}

/// Restore a config file from backup
fn restore_config(config_path: &Path, backup_name: &str) -> Result<bool, String> {
    let backup_path = get_backups_dir().join(backup_name);

    if !backup_path.exists() {
        return Ok(false); // No backup found
    }

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
    }

    fs::copy(&backup_path, config_path)
        .map_err(|e| format!("Failed to restore {}: {}", config_path.display(), e))?;

    Ok(true)
}

// ============================================================================
// Claude Code
// ============================================================================

fn claude_code_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("settings.json")
}

fn detect_claude_code(config_path: &Path) -> bool {
    // Check if .claude directory exists (even if settings.json doesn't)
    config_path.parent().map(|p| p.exists()).unwrap_or(false)
}

fn is_claude_code_configured(config_path: &Path, proxy_url: &str) -> bool {
    if !config_path.exists() {
        return false;
    }

    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(j) => j,
        Err(_) => return false,
    };

    json.get("env")
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(|u| u.as_str())
        .map(|u| u == proxy_url)
        .unwrap_or(false)
}

fn configure_claude_code(config_path: &Path, proxy_url: &str) -> Result<(), String> {
    // Read existing config or create new
    let mut json: serde_json::Value = if config_path.exists() {
        let content =
            fs::read_to_string(config_path).map_err(|e| format!("Failed to read config: {}", e))?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Ensure env object exists
    if json.get("env").is_none() {
        json["env"] = serde_json::json!({});
    }

    // Set AGCP configuration
    json["env"]["ANTHROPIC_BASE_URL"] = serde_json::Value::String(proxy_url.to_string());
    json["env"]["ANTHROPIC_AUTH_TOKEN"] = serde_json::Value::String("agcp".to_string());

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
    }

    // Write config
    let content = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    fs::write(config_path, content).map_err(|e| format!("Failed to write config: {}", e))?;

    Ok(())
}

// ============================================================================
// Codex
// ============================================================================

fn codex_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex")
        .join("config.toml")
}

fn detect_codex(config_path: &Path) -> bool {
    // Check if .codex directory exists
    config_path.parent().map(|p| p.exists()).unwrap_or(false)
}

fn is_codex_configured(config_path: &Path, proxy_url: &str) -> bool {
    if !config_path.exists() {
        return false;
    }

    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Check if model_providers.agcp.base_url is set to our proxy
    let expected_url = format!("{}/v1", proxy_url);
    content.contains(&format!("base_url = \"{}\"", expected_url))
        && content.contains("[model_providers.agcp]")
}

fn configure_codex(config_path: &Path, proxy_url: &str) -> Result<(), String> {
    let openai_url = format!("{}/v1", proxy_url);
    let base_url_line = format!("base_url = \"{}\"", openai_url);

    // Read existing config or create new
    let mut content = if config_path.exists() {
        fs::read_to_string(config_path).map_err(|e| format!("Failed to read config: {}", e))?
    } else {
        String::new()
    };

    // Check if [model_providers.agcp] section already exists
    if content.contains("[model_providers.agcp]") {
        // Update existing base_url in the agcp section
        // This is a simple approach - find and replace the base_url line
        let lines: Vec<&str> = content.lines().collect();
        let mut new_lines: Vec<String> = Vec::new();
        let mut in_agcp_section = false;
        let mut replaced = false;

        for line in lines {
            if line.trim() == "[model_providers.agcp]" {
                in_agcp_section = true;
                new_lines.push(line.to_string());
            } else if line.starts_with('[') && in_agcp_section {
                // Exiting agcp section
                in_agcp_section = false;
                new_lines.push(line.to_string());
            } else if in_agcp_section && line.trim().starts_with("base_url") {
                // Replace the base_url line
                new_lines.push(base_url_line.clone());
                replaced = true;
            } else {
                new_lines.push(line.to_string());
            }
        }

        if !replaced && in_agcp_section {
            // Add base_url if it wasn't found
            new_lines.push(base_url_line.clone());
        }

        content = new_lines.join("\n");
    } else {
        // Add new [model_providers.agcp] section
        let agcp_section = format!(
            r#"
[model_providers.agcp]
name = "AGCP"
base_url = "{}"
"#,
            openai_url
        );
        content.push_str(&agcp_section);
    }

    // Also set model_provider = "agcp" if not already set
    if !content.contains("model_provider = \"agcp\"") {
        // Check if there's an existing model_provider line
        if content.contains("model_provider = ") {
            // Replace it
            content = MODEL_PROVIDER_REGEX
                .replace(&content, "model_provider = \"agcp\"")
                .to_string();
        } else {
            // Add it at the beginning (after any comments)
            let insert_pos = content
                .lines()
                .take_while(|l| l.starts_with('#') || l.trim().is_empty())
                .map(|l| l.len() + 1)
                .sum::<usize>();
            content.insert_str(insert_pos, "model_provider = \"agcp\"\n");
        }
    }

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
    }

    // Write config
    fs::write(config_path, content).map_err(|e| format!("Failed to write config: {}", e))?;

    Ok(())
}

// ============================================================================
// OpenCode
// ============================================================================

fn opencode_config_path() -> PathBuf {
    // OpenCode uses XDG_CONFIG_HOME (~/.config on all platforms), not the
    // platform-native config dir (~/Library/Application Support on macOS).
    xdg_config_dir().join("opencode").join("opencode.json")
}

fn detect_opencode(config_path: &Path) -> bool {
    // Check if opencode config directory exists
    config_path.parent().map(|p| p.exists()).unwrap_or(false)
}

fn is_opencode_configured(config_path: &Path, proxy_url: &str) -> bool {
    if !config_path.exists() {
        return false;
    }

    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(j) => j,
        Err(_) => return false,
    };

    // Check provider.anthropic.options.baseURL
    json.get("provider")
        .and_then(|p| p.get("anthropic"))
        .and_then(|a| a.get("options"))
        .and_then(|o| o.get("baseURL"))
        .and_then(|u| u.as_str())
        .map(|u| u == proxy_url)
        .unwrap_or(false)
}

fn configure_opencode(config_path: &Path, proxy_url: &str) -> Result<(), String> {
    // Read existing config or create new
    let mut json: serde_json::Value = if config_path.exists() {
        let content =
            fs::read_to_string(config_path).map_err(|e| format!("Failed to read config: {}", e))?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Ensure provider.anthropic.options object exists
    if json.get("provider").is_none() {
        json["provider"] = serde_json::json!({});
    }
    if json["provider"].get("anthropic").is_none() {
        json["provider"]["anthropic"] = serde_json::json!({});
    }
    if json["provider"]["anthropic"].get("options").is_none() {
        json["provider"]["anthropic"]["options"] = serde_json::json!({});
    }

    // Set AGCP configuration
    json["provider"]["anthropic"]["options"]["baseURL"] =
        serde_json::Value::String(proxy_url.to_string());

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
    }

    // Write config
    let content = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    fs::write(config_path, content).map_err(|e| format!("Failed to write config: {}", e))?;

    Ok(())
}

// ============================================================================
// Crush
// ============================================================================

fn crush_config_path() -> PathBuf {
    // Crush uses XDG_CONFIG_HOME (~/.config on all platforms), not the
    // platform-native config dir (~/Library/Application Support on macOS).
    xdg_config_dir().join("crush").join("crush.json")
}

fn detect_crush(config_path: &Path) -> bool {
    // Check if crush config directory exists
    config_path.parent().map(|p| p.exists()).unwrap_or(false)
}

fn is_crush_configured(config_path: &Path, proxy_url: &str) -> bool {
    if !config_path.exists() {
        return false;
    }

    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    let json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(j) => j,
        Err(_) => return false,
    };

    // Check providers.anthropic.base_url
    json.get("providers")
        .and_then(|p| p.get("anthropic"))
        .and_then(|a| a.get("base_url"))
        .and_then(|u| u.as_str())
        .map(|u| u == proxy_url)
        .unwrap_or(false)
}

fn configure_crush(config_path: &Path, proxy_url: &str) -> Result<(), String> {
    // Read existing config or create new
    let mut json: serde_json::Value = if config_path.exists() {
        let content =
            fs::read_to_string(config_path).map_err(|e| format!("Failed to read config: {}", e))?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Ensure providers.anthropic object exists
    if json.get("providers").is_none() {
        json["providers"] = serde_json::json!({});
    }
    if json["providers"].get("anthropic").is_none() {
        json["providers"]["anthropic"] = serde_json::json!({});
    }

    // Set AGCP configuration
    json["providers"]["anthropic"]["base_url"] = serde_json::Value::String(proxy_url.to_string());
    json["providers"]["anthropic"]["type"] = serde_json::Value::String("anthropic".to_string());
    // Set a dummy API key (Crush may require one even though AGCP doesn't need it)
    json["providers"]["anthropic"]["api_key"] = serde_json::Value::String("agcp".to_string());

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
    }

    // Write config
    let content = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    fs::write(config_path, content).map_err(|e| format!("Failed to write config: {}", e))?;

    Ok(())
}

// ============================================================================
// Zed
// ============================================================================

fn zed_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("zed")
        .join("settings.json")
}

fn detect_zed(config_path: &Path) -> bool {
    // Check if zed config directory exists
    config_path.parent().map(|p| p.exists()).unwrap_or(false)
}

fn is_zed_configured(config_path: &Path, proxy_url: &str) -> bool {
    if !config_path.exists() {
        return false;
    }

    let content = match fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Zed uses JSONC (JSON with comments) — strip comments before parsing
    let clean = strip_jsonc_comments(&content);
    let json: serde_json::Value = match serde_json::from_str(&clean) {
        Ok(j) => j,
        Err(_) => return false,
    };

    // Check language_models.anthropic.api_url
    json.get("language_models")
        .and_then(|lm| lm.get("anthropic"))
        .and_then(|a| a.get("api_url"))
        .and_then(|u| u.as_str())
        .map(|u| u == proxy_url)
        .unwrap_or(false)
}

fn configure_zed(config_path: &Path, proxy_url: &str) -> Result<(), String> {
    // Read existing config or create new
    let mut json: serde_json::Value = if config_path.exists() {
        let content =
            fs::read_to_string(config_path).map_err(|e| format!("Failed to read config: {}", e))?;
        // Zed uses JSONC — strip comments before parsing
        let clean = strip_jsonc_comments(&content);
        serde_json::from_str(&clean).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    // Ensure language_models.anthropic object exists
    if json.get("language_models").is_none() {
        json["language_models"] = serde_json::json!({});
    }
    if json["language_models"].get("anthropic").is_none() {
        json["language_models"]["anthropic"] = serde_json::json!({});
    }

    // Set api_url
    json["language_models"]["anthropic"]["api_url"] =
        serde_json::Value::String(proxy_url.to_string());

    // Ensure parent directory exists
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {}", e))?;
    }

    // Write config (note: comments from the original file will be lost,
    // but we create a backup before modifying)
    let content = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize config: {}", e))?;
    fs::write(config_path, content).map_err(|e| format!("Failed to write config: {}", e))?;

    Ok(())
}

/// Strip single-line (//) comments from JSONC content.
/// Handles comments inside strings (doesn't strip those).
fn strip_jsonc_comments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escape_next = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if escape_next {
            escape_next = false;
            result.push(ch);
            continue;
        }

        if in_string {
            if ch == '\\' {
                escape_next = true;
            } else if ch == '"' {
                in_string = false;
            }
            result.push(ch);
        } else if ch == '"' {
            in_string = true;
            result.push(ch);
        } else if ch == '/' && chars.peek() == Some(&'/') {
            // Skip until end of line
            for c in chars.by_ref() {
                if c == '\n' {
                    result.push('\n');
                    break;
                }
            }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            // Skip block comment
            chars.next(); // consume '*'
            let mut prev = ' ';
            for c in chars.by_ref() {
                if prev == '*' && c == '/' {
                    break;
                }
                if c == '\n' {
                    result.push('\n'); // preserve line count
                }
                prev = c;
            }
        } else {
            result.push(ch);
        }
    }

    result
}

// ============================================================================
// Main setup command
// ============================================================================

/// Get all supported tools
fn get_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "Claude Code",
            config_path: claude_code_config_path(),
            backup_name: "claude-code.json",
            detect: detect_claude_code,
            is_configured: is_claude_code_configured,
            configure: configure_claude_code,
        },
        Tool {
            name: "Codex",
            config_path: codex_config_path(),
            backup_name: "codex.json",
            detect: detect_codex,
            is_configured: is_codex_configured,
            configure: configure_codex,
        },
        Tool {
            name: "OpenCode",
            config_path: opencode_config_path(),
            backup_name: "opencode.json",
            detect: detect_opencode,
            is_configured: is_opencode_configured,
            configure: configure_opencode,
        },
        Tool {
            name: "Crush",
            config_path: crush_config_path(),
            backup_name: "crush.json",
            detect: detect_crush,
            is_configured: is_crush_configured,
            configure: configure_crush,
        },
        Tool {
            name: "Zed",
            config_path: zed_config_path(),
            backup_name: "zed-settings.json",
            detect: detect_zed,
            is_configured: is_zed_configured,
            configure: configure_zed,
        },
    ]
}

/// Run the setup command
pub fn run_setup_command(args: &[String]) {
    // Check for --undo flag
    if args.iter().any(|a| a == "--undo") {
        run_undo();
        return;
    }

    println!();
    println!("{}{}AGCP Setup{}", BOLD, GREEN, RESET);
    println!();

    let proxy_url = get_proxy_url();
    println!("  Proxy URL: {}{}{}", CYAN, proxy_url, RESET);

    // Warn if daemon is running on a different port than the config file
    let config = Config::load().unwrap_or_default();
    let config_url = format!("http://{}:{}", config.host(), config.port());
    if proxy_url != config_url {
        println!(
            "  {}Note: Daemon is running on {}, which differs from config ({}){}\n",
            YELLOW, proxy_url, config_url, RESET
        );
    }
    println!();
    let tools = get_tools();

    // Detect installed tools
    let detected: Vec<_> = tools
        .iter()
        .filter(|t| (t.detect)(&t.config_path))
        .collect();

    if detected.is_empty() {
        println!("{}No supported tools detected.{}", DIM, RESET);
        println!();
        println!("Supported tools:");
        println!("  • Claude Code  ~/.claude/settings.json");
        println!("  • Codex        ~/.codex/config.json");
        println!("  • OpenCode     ~/.config/opencode/opencode.json");
        println!("  • Crush        ~/.config/crush/crush.json");
        println!("  • Zed          ~/.config/zed/settings.json");
        println!();
        return;
    }

    // Build selection items with status
    let items: Vec<String> = detected
        .iter()
        .map(|t| {
            let configured = (t.is_configured)(&t.config_path, &proxy_url);
            let status = if configured { " (configured)" } else { "" };
            format!("{:<12} {}{}{}", t.name, DIM, t.config_path.display(), RESET) + status
        })
        .collect();

    // Pre-select tools that aren't configured yet
    let defaults: Vec<bool> = detected
        .iter()
        .map(|t| !(t.is_configured)(&t.config_path, &proxy_url))
        .collect();

    // Show interactive selection
    let selections = match MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select tools to configure")
        .items(&items)
        .defaults(&defaults)
        .interact_opt()
    {
        Ok(Some(s)) => s,
        Ok(None) => {
            println!("{}Cancelled.{}", DIM, RESET);
            return;
        }
        Err(e) => {
            eprintln!("{}Error: {}{}", YELLOW, e, RESET);
            return;
        }
    };

    if selections.is_empty() {
        println!("{}No tools selected.{}", DIM, RESET);
        return;
    }

    println!();
    println!("Configuring {} tool(s)...", selections.len());

    // Configure selected tools
    for idx in &selections {
        let tool = detected[*idx];
        print!("  {} ", tool.name);

        // Backup first
        if let Err(e) = backup_config(&tool.config_path, tool.backup_name) {
            println!("{}✗ backup failed: {}{}", YELLOW, e, RESET);
            continue;
        }

        // Configure
        match (tool.configure)(&tool.config_path, &proxy_url) {
            Ok(()) => {
                println!("{}✓{} configured", GREEN, RESET);
                // Show extra instructions for OpenCode
                if tool.name == "OpenCode" {
                    println!(
                        "      {}Note: OpenCode requires ANTHROPIC_API_KEY env var{}",
                        DIM, RESET
                    );
                    println!("      {}Run: export ANTHROPIC_API_KEY=agcp{}", DIM, RESET);
                }
                // Show extra instructions for Zed
                if tool.name == "Zed" {
                    println!(
                        "      {}Note: Set any Anthropic API key in Zed's settings{}",
                        DIM, RESET
                    );
                    println!(
                        "      {}Zed > Settings > Anthropic > API Key (any value works){}",
                        DIM, RESET
                    );
                }
            }
            Err(e) => {
                println!("{}✗ {}{}", YELLOW, e, RESET);
            }
        }
    }

    println!();

    // Verify daemon is reachable at the configured URL
    let (host, port) = crate::config::get_daemon_host_port();
    let addr = format!("{}:{}", host, port);
    let reachable = addr
        .parse::<std::net::SocketAddr>()
        .ok()
        .and_then(|sa| {
            std::net::TcpStream::connect_timeout(&sa, std::time::Duration::from_millis(500)).ok()
        })
        .is_some();

    if reachable {
        println!("{}✓{} Daemon is running at {}", GREEN, RESET, proxy_url);
    } else {
        println!(
            "{}!{} Daemon is not running at {}",
            YELLOW, RESET, proxy_url
        );
        println!("  {}Start it with: agcp{}", DIM, RESET);
    }

    println!();
    println!(
        "{}Done! Run 'agcp setup --undo' to restore previous configs.{}",
        DIM, RESET
    );
    println!();
}

/// Run the undo command
fn run_undo() {
    println!();
    println!("{}{}Restoring configurations...{}", BOLD, GREEN, RESET);
    println!();

    let tools = get_tools();
    let mut restored = 0;

    for tool in &tools {
        print!("  {} ", tool.name);

        match restore_config(&tool.config_path, tool.backup_name) {
            Ok(true) => {
                println!("{}✓{} restored", GREEN, RESET);
                restored += 1;
            }
            Ok(false) => {
                println!("{}○{} no backup found", DIM, RESET);
            }
            Err(e) => {
                println!("{}✗ {}{}", YELLOW, e, RESET);
            }
        }
    }

    println!();
    if restored > 0 {
        println!(
            "{}Done! Restored {} configuration(s).{}",
            DIM, restored, RESET
        );
    } else {
        println!("{}No backups found to restore.{}", DIM, RESET);
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_jsonc_comments() {
        // Line comments
        let input = r#"{
  // This is a comment
  "key": "value"
}"#;
        let clean = strip_jsonc_comments(input);
        let parsed: serde_json::Value = serde_json::from_str(&clean).unwrap();
        assert_eq!(parsed["key"], "value");

        // Block comments
        let input2 = r#"{ /* block */ "key": "value" }"#;
        let clean2 = strip_jsonc_comments(input2);
        let parsed2: serde_json::Value = serde_json::from_str(&clean2).unwrap();
        assert_eq!(parsed2["key"], "value");

        // Comments inside strings should be preserved
        let input3 = r#"{ "key": "http://example.com" }"#;
        let clean3 = strip_jsonc_comments(input3);
        let parsed3: serde_json::Value = serde_json::from_str(&clean3).unwrap();
        assert_eq!(parsed3["key"], "http://example.com");

        // Zed-style config
        let input4 = r#"// Zed settings
{
  "language_models": {
    "anthropic": {
      "api_url": "http://127.0.0.1:3092"
    }
  }
}"#;
        let clean4 = strip_jsonc_comments(input4);
        let parsed4: serde_json::Value = serde_json::from_str(&clean4).unwrap();
        assert_eq!(
            parsed4["language_models"]["anthropic"]["api_url"],
            "http://127.0.0.1:3092"
        );
    }
}
