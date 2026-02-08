use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::app::App;
use crate::tui::theme;

/// Render the accounts view with search bar and sort indicator
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

    // Calculate toolbar height
    let has_search = app.account_search_active || !app.account_search_query.is_empty();
    let has_sort = app.account_sort != crate::tui::app::AccountSort::Default;
    let toolbar_height: u16 = if has_search || has_sort { 2 } else { 0 };

    let toolbar_area = Rect {
        height: toolbar_height,
        ..inner
    };
    let content_area = Rect {
        y: inner.y + toolbar_height,
        height: inner.height.saturating_sub(toolbar_height),
        ..inner
    };

    // Render toolbar if active
    if toolbar_height > 0 {
        render_toolbar(frame, toolbar_area, app);
    }

    // Store content area for mouse click detection
    app.accounts_area = content_area;

    // Determine which accounts to display
    let has_filter = app.has_active_account_filter();
    let display_indices: Vec<usize> = if has_filter {
        app.account_display_indices.clone()
    } else {
        (0..app.accounts.len()).collect()
    };

    if display_indices.is_empty() {
        let no_match = Text::from("No accounts match the search.")
            .style(theme::dim())
            .centered();
        frame.render_widget(no_match, content_area);
        return;
    }

    // Build lines for displayed accounts
    let lines: Vec<Line> = display_indices
        .iter()
        .enumerate()
        .map(|(display_idx, &real_idx)| {
            let acc = &app.accounts[real_idx];
            let is_selected = display_idx == app.account_selected;
            let is_hovered = app.hovered_account == Some(display_idx);

            // Status icon
            let status_icon = if acc.is_invalid {
                ("\u{2717}", theme::error()) // ✗
            } else if !acc.enabled {
                ("\u{25cb}", theme::dim()) // ○
            } else if acc.is_active {
                ("\u{25cf}", theme::success()) // ●
            } else {
                ("\u{25cb}", theme::dim()) // ○
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

            // Quota bar using per-account live data if available
            let quota = app
                .get_account_quota_fraction(&acc.id)
                .unwrap_or(acc.quota_fraction);
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

    frame.render_widget(Paragraph::new(lines), content_area);
}

/// Render search bar and sort indicator
fn render_toolbar(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // First line: sort indicator + filter count
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::styled(" Sort: ", theme::dim()));
    spans.push(Span::styled(
        app.account_sort.label(),
        if app.account_sort != crate::tui::app::AccountSort::Default {
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::dim()
        },
    ));
    spans.push(Span::styled("(s)", Style::default().fg(Color::DarkGray)));

    if app.has_active_account_filter() {
        let displayed = app.account_display_indices.len();
        let total = app.accounts.len();
        spans.push(Span::styled(" \u{2502} ", theme::dim())); // │
        spans.push(Span::styled(
            format!("{}/{}", displayed, total),
            Style::default().fg(theme::WARNING),
        ));
    }

    lines.push(Line::from(spans));

    // Second line: search bar (if active or has query)
    if app.account_search_active || !app.account_search_query.is_empty() {
        let mut search_spans: Vec<Span> = Vec::new();
        search_spans.push(Span::styled(
            " / ",
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ));
        search_spans.push(Span::styled(
            app.account_search_query.clone(),
            if app.account_search_active {
                Style::default().fg(theme::TEXT)
            } else {
                theme::dim()
            },
        ));
        if app.account_search_active {
            search_spans.push(Span::styled(
                "\u{2588}", // █ block cursor
                Style::default().fg(theme::PRIMARY),
            ));
        }
        lines.push(Line::from(search_spans));
    }

    frame.render_widget(Paragraph::new(lines), area);
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
    format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty)) // █ and ░
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
