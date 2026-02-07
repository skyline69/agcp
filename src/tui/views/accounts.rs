use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::app::App;
use crate::tui::theme;

/// Render the accounts view
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .title(" Accounts ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.accounts.is_empty() {
        let empty_msg =
            Text::from("No accounts configured.\n\nRun 'agcp login' to add an account.")
                .style(theme::dim())
                .centered();
        frame.render_widget(empty_msg, inner);
        return;
    }

    // Build lines for each account
    let lines: Vec<Line> = app
        .accounts
        .iter()
        .enumerate()
        .map(|(idx, acc)| {
            let is_selected = idx == app.account_selected;
            let is_hovered = app.hovered_account == Some(idx);

            // Status icon
            let status_icon = if acc.is_invalid {
                ("✗", theme::error())
            } else if !acc.enabled {
                ("○", theme::dim())
            } else if acc.is_active {
                ("●", theme::success())
            } else {
                ("○", theme::dim())
            };

            // Selection indicator
            let selector = if is_selected { "> " } else { "  " };

            // Email style
            let email_style = if acc.is_active {
                theme::primary()
            } else {
                Style::default().fg(theme::TEXT)
            };
            let email_style = if is_hovered {
                email_style.add_modifier(Modifier::UNDERLINED)
            } else {
                email_style
            };

            // Tier badge
            let tier = acc.subscription_tier.as_deref().unwrap_or("free");
            let (tier_text, tier_style) = match tier.to_lowercase().as_str() {
                "ultra" => (
                    "ULTRA",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
                "pro" => (
                    "PRO",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                _ => ("FREE", Style::default().fg(Color::DarkGray)),
            };

            // Quota bar using live data if available
            let quota = if !app.quota_data.is_empty() {
                // Calculate average from live quota
                let total: f64 = app.quota_data.iter().map(|q| q.remaining_fraction).sum();
                total / app.quota_data.len() as f64
            } else {
                acc.quota_fraction
            };
            let quota_bar = render_quota_bar(quota, 10);
            let quota_style = quota_color(quota);

            // Truncate email and calculate padding separately
            let email_display = truncate_email(&acc.email, 32);
            let email_padding = " ".repeat(32_usize.saturating_sub(email_display.len()));

            Line::from(vec![
                Span::raw(selector),
                Span::styled(status_icon.0, status_icon.1),
                Span::raw(" "),
                Span::styled(email_display, email_style),
                Span::raw(email_padding),
                Span::raw(" "),
                Span::styled(format!("{:<5}", tier_text), tier_style),
                Span::raw(" "),
                Span::styled(quota_bar, quota_style),
                Span::styled(format!(" {:>3.0}%", quota * 100.0), theme::dim()),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn truncate_email(email: &str, max_len: usize) -> String {
    if email.len() <= max_len {
        email.to_string()
    } else {
        format!("{}...", &email[..max_len.saturating_sub(3)])
    }
}

fn render_quota_bar(fraction: f64, width: usize) -> String {
    let filled = (fraction * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn quota_color(fraction: f64) -> Style {
    // For quota, LOW remaining = bad (red), HIGH remaining = good (green)
    if fraction <= 0.1 {
        theme::error()
    } else if fraction <= 0.3 {
        theme::warning()
    } else {
        theme::success()
    }
}
