//! Startup warnings popup widget
//!
//! Displays a centered popup with startup diagnostics including config errors,
//! daemon status, and account configuration issues.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::theme;

/// Severity level for startup warnings
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningLevel {
    /// Critical issues that may prevent normal operation (red)
    Error,
    /// Non-critical issues that should be addressed (yellow)
    Warning,
    /// Informational messages (blue)
    Info,
}

impl WarningLevel {
    /// Get the style for this warning level
    pub fn style(&self) -> Style {
        match self {
            WarningLevel::Error => theme::error(),
            WarningLevel::Warning => theme::warning(),
            WarningLevel::Info => Style::default().fg(theme::SECONDARY),
        }
    }

    /// Get the icon for this warning level
    pub fn icon(&self) -> &'static str {
        match self {
            WarningLevel::Error => "✗",
            WarningLevel::Warning => "!",
            WarningLevel::Info => "i",
        }
    }
}

/// A single startup warning or error
#[derive(Debug, Clone)]
pub struct StartupWarning {
    pub level: WarningLevel,
    pub title: String,
    pub message: String,
}

impl StartupWarning {
    pub fn error(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: WarningLevel::Error,
            title: title.into(),
            message: message.into(),
        }
    }

    pub fn warning(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: WarningLevel::Warning,
            title: title.into(),
            message: message.into(),
        }
    }

    pub fn info(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: WarningLevel::Info,
            title: title.into(),
            message: message.into(),
        }
    }
}

/// Collect startup warnings by checking various system states
pub fn collect_startup_warnings() -> Vec<StartupWarning> {
    let mut warnings = Vec::new();

    // Check config file for errors
    let config_ok = check_config(&mut warnings);

    // Only check daemon if config is valid (otherwise we don't know the correct port)
    if config_ok {
        check_daemon(&mut warnings);
    }

    // Check accounts
    check_accounts(&mut warnings);

    // Check log file
    check_log_file(&mut warnings);

    warnings
}

/// Check config file for syntax and validation errors
/// Returns true if config is valid
fn check_config(warnings: &mut Vec<StartupWarning>) -> bool {
    if let Err(e) = crate::config::Config::load() {
        warnings.push(StartupWarning::error("Config Error", e.to_string()));
        false
    } else {
        true
    }
}

/// Check if the daemon is running
fn check_daemon(warnings: &mut Vec<StartupWarning>) {
    use std::net::TcpStream;
    use std::time::Duration;

    let addr = crate::config::get_daemon_addr();

    // Try to connect to the daemon's port
    match TcpStream::connect_timeout(
        &addr
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:8080".parse().unwrap()),
        Duration::from_millis(100),
    ) {
        Ok(_) => {
            // Connection successful - daemon is running
        }
        Err(_) => {
            warnings.push(StartupWarning::warning(
                "Daemon Not Running",
                format!("No daemon listening on {}", addr),
            ));
        }
    }
}

/// Check if accounts are configured
fn check_accounts(warnings: &mut Vec<StartupWarning>) {
    match crate::auth::accounts::AccountStore::load() {
        Ok(store) => {
            if store.accounts.is_empty() {
                warnings.push(StartupWarning::warning(
                    "No Accounts",
                    "Run 'agcp login' to add an account",
                ));
            } else {
                // Check for any enabled, valid accounts
                let valid_count = store
                    .accounts
                    .iter()
                    .filter(|a| a.enabled && !a.is_invalid)
                    .count();

                if valid_count == 0 {
                    warnings.push(StartupWarning::warning(
                        "No Valid Accounts",
                        "All accounts are disabled or invalid",
                    ));
                }

                // Check for invalid accounts
                let invalid: Vec<_> = store
                    .accounts
                    .iter()
                    .filter(|a| a.is_invalid)
                    .map(|a| a.email.clone())
                    .collect();

                if !invalid.is_empty() {
                    warnings.push(StartupWarning::info(
                        "Invalid Accounts",
                        format!("{} account(s) need re-authentication", invalid.len()),
                    ));
                }
            }
        }
        Err(e) => {
            warnings.push(StartupWarning::error("Account Store Error", e.to_string()));
        }
    }
}

/// Check if log file exists
fn check_log_file(warnings: &mut Vec<StartupWarning>) {
    let log_path = crate::tui::data::DataProvider::get_log_path();
    if !log_path.exists() {
        warnings.push(StartupWarning::info(
            "Log File Not Found",
            format!("{}", log_path.display()),
        ));
    }
}

/// Get the highest severity level from a list of warnings
fn max_severity(warnings: &[StartupWarning]) -> WarningLevel {
    warnings
        .iter()
        .map(|w| w.level)
        .max_by_key(|l| match l {
            WarningLevel::Error => 2,
            WarningLevel::Warning => 1,
            WarningLevel::Info => 0,
        })
        .unwrap_or(WarningLevel::Info)
}

/// Render the startup warnings popup
pub fn render(frame: &mut Frame, area: Rect, warnings: &[StartupWarning]) {
    if warnings.is_empty() {
        return;
    }

    // Calculate popup size based on content
    let max_title_len = warnings.iter().map(|w| w.title.len()).max().unwrap_or(20);
    let max_msg_len = warnings.iter().map(|w| w.message.len()).max().unwrap_or(30);
    let content_width = (max_title_len + max_msg_len + 6).max(40); // icon + padding

    // Each warning takes 2 lines (title + message), plus header/footer
    let content_height = warnings.len() * 2 + 3; // +3 for spacing and footer

    let popup_width = (content_width as u16 + 4).min(area.width.saturating_sub(4));
    let popup_height = (content_height as u16 + 2).min(area.height.saturating_sub(4));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    // Determine title and border color based on severity
    let severity = max_severity(warnings);
    let (title, border_style) = match severity {
        WarningLevel::Error => (" Startup Errors ", theme::error()),
        WarningLevel::Warning => (" Startup Issues ", theme::warning()),
        WarningLevel::Info => (" Startup Info ", Style::default().fg(theme::SECONDARY)),
    };

    let block = Block::default()
        .title(title)
        .title_style(border_style.add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .style(theme::surface());

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    // Build content lines
    let mut lines = Vec::new();

    for warning in warnings {
        let icon_style = warning.level.style();
        let icon = warning.level.icon();

        // Title line with icon
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", icon),
                icon_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(&warning.title, icon_style.add_modifier(Modifier::BOLD)),
        ]));

        // Message line (indented)
        lines.push(Line::from(Span::styled(
            format!("   {}", &warning.message),
            theme::dim(),
        )));
    }

    // Add footer
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Press Enter or Esc to dismiss",
        theme::dim(),
    )));

    frame.render_widget(Paragraph::new(lines), inner);
}
