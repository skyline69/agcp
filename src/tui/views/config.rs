use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::app::App;
use crate::tui::config_editor::FieldType;
use crate::tui::theme;

/// Render the config view
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .title(" Configuration ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Store area for mouse click detection
    app.config_area = inner;

    let mut lines: Vec<Line> = Vec::new();
    let mut current_section = "";

    for (idx, field) in app.config_fields.iter().enumerate() {
        // Section header
        if field.section != current_section {
            if !current_section.is_empty() {
                lines.push(Line::from("")); // Blank line between sections
            }
            lines.push(Line::from(Span::styled(
                format!("[{}]", field.section),
                theme::primary(),
            )));
            current_section = field.section;
        }

        let is_selected = idx == app.config_selected;
        let is_hovered = app.hovered_config == Some(idx);
        let is_modified = field.is_modified();

        // Selection indicator
        let selector = if is_selected { "> " } else { "  " };

        // Key name (padded) - bold if selected, underlined if hovered
        let key_style = if is_selected {
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD)
        } else if is_hovered {
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default().fg(theme::TEXT)
        };

        // Value display
        let value_display = if app.config_editing && is_selected {
            // Show edit buffer with cursor
            format!("{}█", app.config_edit_buffer)
        } else {
            format_value(field)
        };

        let value_style = match &field.field_type {
            FieldType::Bool => {
                if field.value == "true" {
                    theme::success()
                } else {
                    theme::dim()
                }
            }
            FieldType::Enum(_) => theme::primary(),
            _ => theme::success(),
        };

        // Modified indicator
        let modified = if is_modified { " [*]" } else { "" };

        // Calculate padding separately so we don't underline it
        let key_width: usize = 24;
        let key_padding = " ".repeat(key_width.saturating_sub(field.key.len()));

        // Build the line
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(
                selector,
                if is_selected {
                    theme::primary()
                } else {
                    theme::dim()
                },
            ),
            Span::styled(field.key.to_string(), key_style),
            Span::raw(key_padding),
            Span::styled(value_display, value_style),
        ];

        // Add enum cycling hint for selected enum fields
        if is_selected
            && !app.config_editing
            && let FieldType::Enum(_) = &field.field_type
        {
            spans.push(Span::styled("  ◀ ▶", theme::dim()));
        }

        if is_modified {
            spans.push(Span::styled(modified, theme::warning()));
        }

        lines.push(Line::from(spans));
    }

    // Status line at bottom
    lines.push(Line::from(""));

    if let Some(ref error) = app.config_error {
        lines.push(Line::from(Span::styled(
            format!("  Error: {}", error),
            theme::error(),
        )));
    } else if app.config_needs_restart {
        lines.push(Line::from(Span::styled(
            "  Config saved. Press 'r' to restart daemon.",
            theme::success(),
        )));
    } else if app.config_has_changes() {
        lines.push(Line::from(Span::styled(
            "  Unsaved changes. Press 's' to save.",
            theme::warning(),
        )));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

fn format_value(field: &crate::tui::config_editor::ConfigField) -> String {
    match &field.field_type {
        FieldType::Text => {
            // Quote strings that aren't numbers
            if field.value.parse::<f64>().is_ok() {
                field.value.clone()
            } else {
                format!("\"{}\"", field.value)
            }
        }
        FieldType::Bool | FieldType::Float { .. } | FieldType::Enum(_) => field.value.clone(),
    }
}
