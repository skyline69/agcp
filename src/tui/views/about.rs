//! About view with animated ASCII logo and project info.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::app::UpdateStatus;
use crate::tui::theme;

/// ASCII art logo for AGCP (from CLI help)
const LOGO: &[&str] = &[
    " ▗▄▖  ▗▄▄▖ ▗▄▄▖▗▄▄▖ ",
    "▐▌ ▐▌▐▌   ▐▌   ▐▌ ▐▌",
    "▐▛▀▜▌▐▌▝▜▌▐▌   ▐▛▀▘ ",
    "▐▌ ▐▌▝▚▄▞▘▝▚▄▄▖▐▌   ",
];

/// Project version from Cargo.toml
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// GitHub URL
pub const GITHUB_URL: &str = "https://github.com/skyline69/agcp";

/// Render the about view
pub fn render(
    frame: &mut Frame,
    area: Rect,
    animation_time_ms: u64,
    hovered_link: bool,
    update_status: &UpdateStatus,
) {
    let block = Block::default()
        .title(" About ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    // Calculate vertical centering
    let logo_height = LOGO.len();
    let info_height = 11; // Info lines below logo (including update status)
    let total_height = logo_height + info_height + 2; // +2 for spacing
    let start_y = inner.height.saturating_sub(total_height as u16) / 2;

    // Add top padding
    for _ in 0..start_y {
        lines.push(Line::from(""));
    }

    // Render logo with rainbow animation
    for (line_idx, logo_line) in LOGO.iter().enumerate() {
        let mut spans = Vec::new();

        // Calculate horizontal centering
        let logo_width = logo_line.chars().count();
        let padding = (inner.width as usize).saturating_sub(logo_width) / 2;
        spans.push(Span::raw(" ".repeat(padding)));

        // Render each character with rainbow color
        for (char_idx, ch) in logo_line.chars().enumerate() {
            if ch == ' ' {
                spans.push(Span::raw(" "));
            } else {
                // Offset by line and char index for wave effect
                let offset = line_idx * 2 + char_idx;
                let style = theme::rainbow_style(animation_time_ms, offset);
                spans.push(Span::styled(ch.to_string(), style));
            }
        }

        lines.push(Line::from(spans));
    }

    // Spacing after logo
    lines.push(Line::from(""));
    lines.push(Line::from(""));

    // Project info - centered
    let title = "Extremely Lightweight Antigravity-Claude-Proxy";
    let title_padding = (inner.width as usize).saturating_sub(title.len()) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(title_padding)),
        Span::styled(title, theme::primary()),
    ]));

    lines.push(Line::from(""));

    let version_text = format!("Version {}", VERSION);
    let version_padding = (inner.width as usize).saturating_sub(version_text.len()) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(version_padding)),
        Span::styled(version_text, theme::dim()),
    ]));

    lines.push(Line::from(""));

    let desc_lines = [
        "A proxy that translates Anthropic's Claude API",
        "to Google's Cloud Code API, enabling Claude and",
        "Gemini models through an Anthropic-compatible interface.",
    ];

    for desc in desc_lines {
        let desc_padding = (inner.width as usize).saturating_sub(desc.len()) / 2;
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(desc_padding)),
            Span::styled(desc, theme::dim()),
        ]));
    }

    lines.push(Line::from(""));

    // GitHub link with hover effect
    let link_style = if hovered_link {
        Style::default()
            .fg(theme::SECONDARY)
            .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
    } else {
        Style::default().fg(theme::SECONDARY)
    };

    let link_padding = (inner.width as usize).saturating_sub(GITHUB_URL.len()) / 2;
    lines.push(Line::from(vec![
        Span::raw(" ".repeat(link_padding)),
        Span::styled(GITHUB_URL, link_style),
    ]));

    // Update status
    lines.push(Line::from(""));
    let (update_text, update_style) = match update_status {
        UpdateStatus::NotChecked => ("", Style::default()),
        UpdateStatus::Checking => ("Checking for updates...", theme::dim()),
        UpdateStatus::UpToDate => (
            "Up to date",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        UpdateStatus::UpdateAvailable { .. } | UpdateStatus::Error(_) => {
            // Handled below with dynamic text
            ("", Style::default())
        }
    };

    match update_status {
        UpdateStatus::UpdateAvailable { current, latest } => {
            let text = format!("Update available: v{} -> v{}", current, latest);
            let padding = (inner.width as usize).saturating_sub(text.len()) / 2;
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(padding)),
                Span::styled(
                    text,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        }
        UpdateStatus::Error(msg) => {
            let text = format!("Update check failed: {}", msg);
            let display = if text.len() > inner.width as usize - 4 {
                format!("{}...", &text[..inner.width as usize - 7])
            } else {
                text
            };
            let padding = (inner.width as usize).saturating_sub(display.len()) / 2;
            lines.push(Line::from(vec![
                Span::raw(" ".repeat(padding)),
                Span::styled(display, theme::error()),
            ]));
        }
        _ => {
            if !update_text.is_empty() {
                let padding = (inner.width as usize).saturating_sub(update_text.len()) / 2;
                lines.push(Line::from(vec![
                    Span::raw(" ".repeat(padding)),
                    Span::styled(update_text, update_style),
                ]));
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Calculate the row where the GitHub link is displayed (relative to inner area)
pub fn get_link_row(inner_height: u16) -> u16 {
    let logo_height = LOGO.len() as u16;
    let info_height = 11u16;
    let total_height = logo_height + info_height + 2;
    let start_y = inner_height.saturating_sub(total_height) / 2;

    // Link is at: start_y + logo_height + 2 (spacing) + 8 (info lines before link)
    start_y + logo_height + 2 + 8
}

/// Open a URL in the default browser
pub fn open_url(url: &str) {
    use std::process::Stdio;

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}
