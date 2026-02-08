use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::app::App;
use crate::tui::theme;

/// Render the mappings view with preset selector, background model, and rule list
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .title(" Mappings ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 3 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // Header: Preset selector
    let preset_label = app.mapping_preset.label();
    let preset_desc = app.mapping_preset.description();
    lines.push(Line::from(vec![
        Span::styled(" Preset: ", theme::dim()),
        Span::styled(
            preset_label,
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  (p)", Style::default().fg(Color::DarkGray)),
        Span::styled("  ", theme::dim()),
        Span::styled(preset_desc, Style::default().fg(Color::DarkGray)),
    ]));

    // Background task model
    lines.push(Line::from(vec![
        Span::styled(" Background: ", theme::dim()),
        Span::styled(
            app.mapping_background_model.clone(),
            Style::default()
                .fg(theme::PRIMARY)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  (b)", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "  Model for CLI background tasks",
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    // Blank separator
    lines.push(Line::from(""));

    // Rules header
    let rule_count = app.mapping_rules.len();
    lines.push(Line::from(vec![Span::styled(
        format!(
            " Rules ({} mapping{})",
            rule_count,
            if rule_count == 1 { "" } else { "s" }
        ),
        Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD),
    )]));

    // Separator line
    let sep_width = inner.width.saturating_sub(2) as usize;
    lines.push(Line::from(Span::styled(
        format!(" {}", "\u{2500}".repeat(sep_width)),
        theme::dim(),
    )));

    // Track where rules start for mouse hit testing
    let rules_start_row = lines.len() as u16;

    // Render each rule
    if app.mapping_rules.is_empty() {
        lines.push(Line::from(Span::styled(
            "   No mapping rules. Press 'a' to add one, or 'p' to select a preset.",
            theme::dim(),
        )));
    } else {
        // Calculate column widths
        let max_from = app
            .mapping_rules
            .iter()
            .map(|r| r.from.len())
            .max()
            .unwrap_or(10)
            .max(10);
        let from_width = max_from.min(30); // Cap at 30 chars

        for (idx, rule) in app.mapping_rules.iter().enumerate() {
            let is_selected = idx == app.mapping_selected;
            let is_hovered = app.hovered_mapping == Some(idx);

            let selector = if is_selected { "\u{25b8} " } else { "  " };

            let from_display = if app.mapping_editing_from && is_selected {
                // Show edit buffer with cursor
                format!("{}\u{2588}", app.mapping_edit_buffer)
            } else {
                rule.from.clone()
            };

            let from_style = if app.mapping_editing_from && is_selected {
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::UNDERLINED)
            } else if is_selected {
                Style::default()
                    .fg(theme::PRIMARY)
                    .add_modifier(Modifier::BOLD)
            } else if is_hovered {
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(theme::TEXT)
            };

            let to_style = if is_selected {
                Style::default()
                    .fg(theme::PRIMARY)
                    .add_modifier(Modifier::BOLD)
            } else if is_hovered {
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::UNDERLINED)
            } else {
                Style::default().fg(theme::TEXT)
            };

            // Pad "from" column
            let from_padded = if from_display.len() > from_width + 1 {
                // +1 for cursor char when editing
                format!("{}...", &from_display[..from_width.saturating_sub(3)])
            } else {
                from_display.clone()
            };
            let padding = " ".repeat((from_width + 1).saturating_sub(from_padded.len()));

            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    selector,
                    if is_selected {
                        theme::primary()
                    } else {
                        theme::dim()
                    },
                ),
                Span::styled(from_padded, from_style),
                Span::raw(padding),
                Span::styled(" \u{2192} ", theme::dim()), // →
                Span::styled(rule.to.clone(), to_style),
            ]));
        }
    }

    // Status line (if any)
    if let Some(ref status) = app.mapping_status {
        lines.push(Line::from(""));
        let status_style = if status.starts_with("Error") {
            theme::error()
        } else {
            theme::success()
        };
        lines.push(Line::from(Span::styled(
            format!(" {}", status),
            status_style,
        )));
    } else if app.mapping_dirty {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Unsaved changes (s to save)",
            Style::default().fg(theme::WARNING),
        )));
    }

    // Store the rules area for mouse detection (offset from inner area)
    app.mapping_area = Rect {
        x: inner.x,
        y: inner.y + rules_start_row,
        width: inner.width,
        height: inner.height.saturating_sub(rules_start_row),
    };

    frame.render_widget(Paragraph::new(lines), inner);
}
