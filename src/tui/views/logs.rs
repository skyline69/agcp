use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, Wrap,
};

use crate::tui::app::App;
use crate::tui::data::LogLevel;
use crate::tui::theme;

/// Render the logs view with scrollbar
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

    if app.logs.is_empty() {
        let empty_msg = Text::from("No logs yet. Start the server with 'agcp'")
            .style(theme::dim())
            .centered();
        frame.render_widget(empty_msg, inner);
        return;
    }

    // Only highlight the visible window of log entries (not all 1000+)
    let visible_height = inner.height as usize;
    let total_lines = app.logs.len();

    // Calculate scroll offset based on app.log_scroll (0 = bottom/newest)
    // log_scroll is how many lines we've scrolled UP from the bottom
    let scroll_offset = total_lines
        .saturating_sub(visible_height)
        .saturating_sub(app.log_scroll);

    // Build styled text only for the visible window
    let lines: Vec<Line> = app
        .logs
        .iter()
        .skip(scroll_offset)
        .take(visible_height)
        .map(|entry| highlight_log_line(&entry.line, entry.level))
        .collect();

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });

    // Reserve space for scrollbar
    let logs_area = Rect {
        width: inner.width.saturating_sub(1),
        ..inner
    };
    frame.render_widget(paragraph, logs_area);

    // Render scrollbar
    let scrollbar_area = Rect {
        x: inner.x + inner.width.saturating_sub(1),
        y: inner.y,
        width: 1,
        height: inner.height,
    };

    // Store scrollbar area for drag detection
    app.scrollbar_area = scrollbar_area;

    // Update scrollbar state
    // ratatui's Scrollbar calculates thumb position using:
    //   thumb_start = position * track_length / (content_length - 1 + viewport)
    // For thumb to reach the bottom, position must approach content_length - 1.
    //
    // Our scroll_offset ranges from 0 to (total_lines - visible_height).
    // We need to map this to 0 to (total_lines - 1) for the scrollbar.
    let max_scroll_offset = total_lines.saturating_sub(visible_height);
    let position = if max_scroll_offset == 0 {
        0
    } else {
        // Scale scroll_offset to the scrollbar's position range
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
        "error" => Style::default().fg(Color::Red),
        _ => Style::default().fg(theme::TEXT),
    }
}
