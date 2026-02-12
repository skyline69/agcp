//! Runtime warning popup widget.
//!
//! Displays high-priority warnings detected while the daemon is running.

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::theme;

/// Render a centered runtime warning popup.
pub fn render(frame: &mut Frame, area: Rect, message: &str) {
    let popup_width = 90.min(area.width.saturating_sub(4));
    let popup_height = 10.min(area.height.saturating_sub(4));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Runtime Warning ")
        .title_style(theme::warning().add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::warning())
        .style(theme::surface());

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let lines = vec![
        Line::from(Span::styled("Gemini Access Disabled", theme::warning())),
        Line::from(""),
        Line::from(Span::styled(message, Style::default().fg(theme::TEXT))),
        Line::from(""),
        Line::from(Span::styled("Press Enter or Esc to dismiss", theme::dim())),
    ];

    frame.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: true }),
        inner,
    );
}
