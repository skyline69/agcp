use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, Wrap,
};

use crate::tui::app::App;
use crate::tui::data::LogLevel;
use crate::tui::theme;

/// Render the logs view with toolbar, search bar, and scrollbar
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .title(" Logs ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Calculate toolbar height: 1 for filter bar + 1 for search bar (if active)
    let toolbar_height: u16 = if app.log_search_active || !app.log_search_query.is_empty() {
        2
    } else {
        1
    };

    // Split inner area into toolbar and log content
    let toolbar_area = Rect {
        height: toolbar_height,
        ..inner
    };
    let content_area = Rect {
        y: inner.y + toolbar_height,
        height: inner.height.saturating_sub(toolbar_height),
        ..inner
    };

    // Render the filter toolbar (stores hit-test rects in app)
    render_toolbar(frame, toolbar_area, app);

    if app.logs.is_empty() {
        let empty_msg = Text::from("No logs yet. Start the server with 'agcp'")
            .style(theme::dim())
            .centered();
        frame.render_widget(empty_msg, content_area);
        return;
    }

    // Determine which entries to display
    let has_filter = app.has_active_log_filter();
    let total_lines = if has_filter {
        app.log_filtered_indices.len()
    } else {
        app.logs.len()
    };

    let visible_height = content_area.height as usize;

    // Calculate scroll offset based on app.log_scroll (0 = bottom/newest)
    let scroll_offset = total_lines
        .saturating_sub(visible_height)
        .saturating_sub(app.log_scroll);

    // Build styled text only for the visible window
    let lines: Vec<Line> = if has_filter {
        app.log_filtered_indices
            .iter()
            .skip(scroll_offset)
            .take(visible_height)
            .filter_map(|&idx| app.logs.get(idx))
            .map(|entry| highlight_log_line(&entry.line, entry.level))
            .collect()
    } else {
        app.logs
            .iter()
            .skip(scroll_offset)
            .take(visible_height)
            .map(|entry| highlight_log_line(&entry.line, entry.level))
            .collect()
    };

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });

    // Reserve space for scrollbar
    let logs_area = Rect {
        width: content_area.width.saturating_sub(1),
        ..content_area
    };
    frame.render_widget(paragraph, logs_area);

    // Render scrollbar
    let scrollbar_area = Rect {
        x: content_area.x + content_area.width.saturating_sub(1),
        y: content_area.y,
        width: 1,
        height: content_area.height,
    };

    // Store scrollbar area for drag detection
    app.scrollbar_area = scrollbar_area;

    // Update scrollbar state
    let max_scroll_offset = total_lines.saturating_sub(visible_height);
    let position = if max_scroll_offset == 0 {
        0
    } else {
        let max_position = total_lines.saturating_sub(1);
        scroll_offset * max_position / max_scroll_offset
    };

    app.log_scrollbar_state = app
        .log_scrollbar_state
        .content_length(total_lines)
        .viewport_content_length(visible_height)
        .position(position);

    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("▲"))
        .end_symbol(Some("▼"))
        .track_symbol(Some("│"))
        .thumb_symbol("█")
        .style(theme::dim());

    frame.render_stateful_widget(scrollbar, scrollbar_area, &mut app.log_scrollbar_state);

    // Render account dropdown popup if open
    if app.log_account_dropdown_open {
        render_account_dropdown(frame, area, app);
    }
}

/// Render the filter toolbar row, storing hit-test rects for mouse interaction
fn render_toolbar(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 {
        return;
    }

    let first_row = Rect { height: 1, ..area };
    let mut spans: Vec<Span> = Vec::new();

    // Track x position for computing hit-test rects
    let mut x_pos: u16 = area.x;

    // Leading space
    spans.push(Span::styled(" ", theme::dim()));
    x_pos += 1;

    // Level filter badges - track each badge's area
    let levels: [(char, &str, usize, Color); 4] = [
        ('d', "Debug", 0, Color::DarkGray),
        ('i', "Info", 1, Color::Green),
        ('w', "Warn", 2, Color::Yellow),
        ('e', "Error", 3, Color::Red),
    ];

    for (key, label, idx, color) in &levels {
        let enabled = app.log_level_filter[*idx];
        let is_hovered = app.hovered_log_level == Some(*idx);

        // Each badge: "■ Label(k) " = 2 + label.len() + 3 + 1 chars
        let badge_start = x_pos;
        let indicator = if enabled { "\u{25a0}" } else { "\u{25a1}" }; // ■ or □

        let badge_style = if is_hovered {
            Style::default()
                .fg(if enabled { *color } else { Color::White })
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else if enabled {
            Style::default().fg(*color).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let label_style = if is_hovered {
            Style::default()
                .fg(if enabled { *color } else { Color::White })
                .add_modifier(Modifier::UNDERLINED)
        } else if enabled {
            Style::default().fg(*color)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        spans.push(Span::styled(format!("{} ", indicator), badge_style));
        x_pos += 2;
        spans.push(Span::styled(label.to_string(), label_style));
        x_pos += label.len() as u16;
        spans.push(Span::styled(
            format!("({})", key),
            if is_hovered {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
        x_pos += 3;
        spans.push(Span::raw(" "));
        x_pos += 1;

        // Store this badge's hit-test area
        let badge_width = x_pos - badge_start;
        app.log_level_badge_areas[*idx] = Rect {
            x: badge_start,
            y: first_row.y,
            width: badge_width,
            height: 1,
        };
    }

    // Separator
    spans.push(Span::styled("\u{2502} ", theme::dim())); // │
    x_pos += 2;

    // Account filter
    let account_start = x_pos;
    let account_label = match &app.log_account_filter {
        None => "All Accounts".to_string(),
        Some(email) => email.clone(),
    };

    let is_account_hovered = app.hovered_log_account;

    let account_prefix_style = if is_account_hovered {
        Style::default()
            .fg(theme::PRIMARY)
            .add_modifier(Modifier::UNDERLINED)
    } else {
        theme::dim()
    };

    let account_value_style = if is_account_hovered {
        Style::default()
            .fg(theme::PRIMARY)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else if app.log_account_filter.is_some() {
        Style::default()
            .fg(theme::PRIMARY)
            .add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    };

    spans.push(Span::styled("Account: ", account_prefix_style));
    x_pos += 9;
    spans.push(Span::styled(account_label.clone(), account_value_style));
    x_pos += account_label.len() as u16;

    let arrow_style = if is_account_hovered {
        Style::default().fg(theme::PRIMARY)
    } else {
        theme::dim()
    };
    spans.push(Span::styled(
        if app.log_account_dropdown_open {
            " \u{25b2}" // ▲
        } else {
            " \u{25bc}" // ▼
        },
        arrow_style,
    ));
    x_pos += 2;

    // Store account filter hit-test area
    app.log_account_filter_area = Rect {
        x: account_start,
        y: first_row.y,
        width: x_pos - account_start,
        height: 1,
    };

    // Show filtered count if filters are active
    if app.has_active_log_filter() {
        let filtered = app.log_filtered_indices.len();
        let total = app.logs.len();
        spans.push(Span::styled(" \u{2502} ", theme::dim())); // │
        spans.push(Span::styled(
            format!("{}/{}", filtered, total),
            Style::default().fg(theme::WARNING),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), first_row);

    // Second row: search bar (if active or has query)
    if area.height >= 2 && (app.log_search_active || !app.log_search_query.is_empty()) {
        let search_row = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };

        // Store search area for click detection
        app.log_search_area = search_row;

        let mut search_spans: Vec<Span> = Vec::new();
        search_spans.push(Span::styled(
            " / ",
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ));
        search_spans.push(Span::styled(
            app.log_search_query.clone(),
            if app.log_search_active {
                Style::default().fg(theme::TEXT)
            } else {
                theme::dim()
            },
        ));
        if app.log_search_active {
            // Cursor
            search_spans.push(Span::styled(
                "\u{2588}", // █ block cursor
                Style::default().fg(theme::PRIMARY),
            ));
        }

        frame.render_widget(Paragraph::new(Line::from(search_spans)), search_row);
    } else {
        app.log_search_area = Rect::default();
    }
}

/// Render the account filter dropdown popup, storing hit-test rects
fn render_account_dropdown(frame: &mut Frame, area: Rect, app: &mut App) {
    let emails = app.log_account_emails();
    let item_count = emails.len() + 1; // +1 for "All Accounts"
    let dropdown_height = (item_count as u16 + 2).min(area.height.saturating_sub(4)); // +2 for borders
    let dropdown_width = emails.iter().map(|e| e.len()).max().unwrap_or(12).max(14) as u16 + 6; // padding + borders + prefix

    // Position dropdown below the account filter label
    let dropdown_x = app.log_account_filter_area.x;
    let dropdown_y = app.log_account_filter_area.y + 1;

    let dropdown_area = Rect {
        x: dropdown_x.min(area.x + area.width.saturating_sub(dropdown_width)),
        y: dropdown_y,
        width: dropdown_width.min(area.width),
        height: dropdown_height,
    };

    // Store dropdown area for mouse detection
    app.log_dropdown_area = dropdown_area;

    let block = Block::default()
        .title(" Account ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border_focused())
        .style(theme::surface());

    let inner = block.inner(dropdown_area);

    // Clear the area behind the dropdown
    frame.render_widget(Clear, dropdown_area);
    frame.render_widget(block, dropdown_area);

    // Store item areas for mouse hit-testing
    app.log_dropdown_item_areas.clear();

    // Render items
    let mut lines: Vec<Line> = Vec::new();

    // "All Accounts" option
    let is_hovered = app.hovered_log_dropdown_item == Some(0);
    let is_selected = app.log_account_dropdown_selected == 0;
    let is_current = app.log_account_filter.is_none();
    let all_style = if is_hovered || is_selected {
        Style::default()
            .fg(theme::PRIMARY)
            .add_modifier(Modifier::BOLD)
    } else if is_current {
        Style::default().fg(theme::TEXT)
    } else {
        theme::dim()
    };
    let bg_style = if is_hovered || is_selected {
        Style::default().bg(Color::Rgb(30, 40, 55))
    } else {
        Style::default()
    };
    let prefix = if is_current {
        "\u{25cf} " // ●
    } else {
        "  "
    };
    lines.push(Line::from(vec![
        Span::styled(prefix, all_style.patch(bg_style)),
        Span::styled("All Accounts", all_style.patch(bg_style)),
    ]));

    app.log_dropdown_item_areas.push(Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    });

    // Individual accounts
    for (i, email) in emails.iter().enumerate() {
        let is_hovered = app.hovered_log_dropdown_item == Some(i + 1);
        let is_selected = app.log_account_dropdown_selected == i + 1;
        let is_current = app.log_account_filter.as_ref() == Some(email);
        let style = if is_hovered || is_selected {
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default().fg(theme::TEXT)
        } else {
            theme::dim()
        };
        let bg_style = if is_hovered || is_selected {
            Style::default().bg(Color::Rgb(30, 40, 55))
        } else {
            Style::default()
        };
        let prefix = if is_current { "\u{25cf} " } else { "  " }; // ●
        lines.push(Line::from(vec![
            Span::styled(prefix, style.patch(bg_style)),
            Span::styled(email.clone(), style.patch(bg_style)),
        ]));

        let item_y = inner.y + (i as u16) + 1;
        if item_y < inner.y + inner.height {
            app.log_dropdown_item_areas.push(Rect {
                x: inner.x,
                y: item_y,
                width: inner.width,
                height: 1,
            });
        }
    }

    let content = Paragraph::new(lines);
    frame.render_widget(content, inner);
}

/// Highlight a log line with syntax coloring
/// Note: `line` is expected to already be ANSI-stripped (LogEntry::new handles this)
pub fn highlight_log_line(line: &str, level: LogLevel) -> Line<'static> {
    let mut spans: Vec<Span> = Vec::new();

    // Parse the structured log format directly (no ANSI stripping needed,
    // LogEntry::new() already stripped ANSI codes before storing)
    let mut remaining = line;

    // Parse timestamp (everything up to the log level)
    if let Some(level_pos) = find_log_level_pos(remaining) {
        let timestamp = &remaining[..level_pos];
        spans.push(Span::styled(timestamp.to_string(), theme::dim()));
        remaining = &remaining[level_pos..];
    }

    // Parse log level
    let level_style = match level {
        LogLevel::Debug => Style::default().fg(Color::DarkGray),
        LogLevel::Info => Style::default().fg(Color::Green),
        LogLevel::Warn => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        LogLevel::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    };

    if let Some(end) = remaining.find(' ') {
        let level_str = &remaining[..end];
        spans.push(Span::styled(level_str.to_string(), level_style));
        remaining = &remaining[end..];
    }

    // Parse the rest as key=value pairs and message text
    parse_key_value_pairs(remaining, &mut spans);

    Line::from(spans)
}

/// Find the position of the log level in the line
fn find_log_level_pos(line: &str) -> Option<usize> {
    // Log levels appear after timestamp, preceded by spaces
    for level in &[
        " INFO ", "INFO ", " WARN ", "WARN ", " ERROR ", "ERROR ", " DEBUG ", "DEBUG ",
    ] {
        if let Some(pos) = line.find(level) {
            // Return position after leading spaces
            let trimmed_pos = pos + level.len() - level.trim_start().len();
            return Some(trimmed_pos);
        }
    }
    None
}

/// Parse key=value pairs and colorize them
fn parse_key_value_pairs(text: &str, spans: &mut Vec<Span<'static>>) {
    let mut chars = text.char_indices().peekable();
    let mut current_start = 0;
    let mut in_key = false;
    let mut key_start = 0;

    while let Some((i, c)) = chars.next() {
        if c == '=' && !in_key {
            // Found a key=value pair
            // Look back to find the start of the key (after space)
            let before = &text[current_start..i];
            if let Some(space_pos) = before.rfind(' ') {
                // Text before the key
                let prefix = &text[current_start..current_start + space_pos + 1];
                if !prefix.is_empty() {
                    spans.push(Span::styled(
                        prefix.to_string(),
                        Style::default().fg(theme::TEXT),
                    ));
                }
                key_start = current_start + space_pos + 1;
            } else {
                key_start = current_start;
            }
            in_key = true;
        } else if in_key && (c == ' ' || chars.peek().is_none()) {
            // End of value
            let key = &text[key_start
                ..text[key_start..]
                    .find('=')
                    .map(|p| key_start + p)
                    .unwrap_or(i)];
            let eq_pos = key_start + key.len();
            let value_end = if c == ' ' { i } else { i + c.len_utf8() };
            let value = &text[eq_pos + 1..value_end];

            // Style the key
            spans.push(Span::styled(
                key.to_string(),
                Style::default().fg(Color::Cyan),
            ));
            spans.push(Span::styled(
                "=".to_string(),
                Style::default().fg(Color::DarkGray),
            ));

            // Style the value based on content
            let value_style = get_value_style(key, value);
            spans.push(Span::styled(value.to_string(), value_style));

            if c == ' ' {
                spans.push(Span::raw(" ".to_string()));
            }

            current_start = value_end + if c == ' ' { 1 } else { 0 };
            in_key = false;
        }
    }

    // Any remaining text
    if current_start < text.len() {
        let remaining = &text[current_start..];
        if !remaining.is_empty() {
            spans.push(Span::styled(
                remaining.to_string(),
                Style::default().fg(theme::TEXT),
            ));
        }
    }
}

/// Get style for a value based on the key name and value content
fn get_value_style(key: &str, value: &str) -> Style {
    match key {
        "model" => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        "status" => {
            if value.starts_with('2') {
                Style::default().fg(Color::Green)
            } else if value.starts_with('4') {
                Style::default().fg(Color::Yellow)
            } else if value.starts_with('5') {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(theme::TEXT)
            }
        }
        "method" => Style::default()
            .fg(Color::Blue)
            .add_modifier(Modifier::BOLD),
        "path" => Style::default().fg(Color::White),
        "duration_ms" => {
            // Color based on duration
            if let Ok(ms) = value.parse::<u64>() {
                if ms < 1000 {
                    Style::default().fg(Color::Green)
                } else if ms < 5000 {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Red)
                }
            } else {
                Style::default().fg(theme::TEXT)
            }
        }
        "request_id" | "agent" => Style::default().fg(Color::DarkGray),
        "address" | "port" => Style::default().fg(Color::Cyan),
        "account" => Style::default().fg(Color::Magenta),
        "error" => Style::default().fg(Color::Red),
        _ => Style::default().fg(theme::TEXT),
    }
}
