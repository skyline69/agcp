use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::theme;

/// Render a help overlay in the center of the screen
pub fn render(frame: &mut Frame, area: Rect) {
    // Calculate centered popup size
    let popup_width = 50.min(area.width.saturating_sub(4));
    let popup_height = 30.min(area.height.saturating_sub(4));

    let popup_area = Rect {
        x: area.x + (area.width.saturating_sub(popup_width)) / 2,
        y: area.y + (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Help ")
        .title_style(theme::primary().add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::primary())
        .style(theme::surface());

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let help_text = vec![
        Line::from(Span::styled("Navigation", theme::primary())),
        Line::from("  Tab / < >     Switch tabs"),
        Line::from("  1-7           Jump to tab"),
        Line::from("  ^ v / j k     Navigate lists"),
        Line::from(""),
        Line::from(Span::styled("Overview Tab", theme::primary())),
        Line::from("  s             Start daemon"),
        Line::from("  x             Stop daemon"),
        Line::from("  r             Restart daemon"),
        Line::from(""),
        Line::from(Span::styled("Logs Tab", theme::primary())),
        Line::from("  /             Search logs"),
        Line::from("  d i w e       Toggle log levels"),
        Line::from("  a             Filter by account"),
        Line::from("  c             Clear filters"),
        Line::from(""),
        Line::from(Span::styled("Accounts Tab", theme::primary())),
        Line::from("  Enter         Set as active"),
        Line::from("  e             Toggle enabled"),
        Line::from("  /             Search accounts"),
        Line::from("  s             Cycle sort"),
        Line::from("  c             Clear filters"),
        Line::from("  r             Refresh"),
        Line::from(""),
        Line::from(Span::styled("General", theme::primary())),
        Line::from("  ?             Toggle help"),
        Line::from("  q / Esc       Quit"),
    ];

    frame.render_widget(Paragraph::new(help_text), inner);
}
