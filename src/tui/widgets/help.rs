use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::tui::theme;

/// Render a help overlay in the center of the screen
pub fn render(frame: &mut Frame, area: Rect) {
    // Two-column layout: wider but shorter
    let popup_width = 80.min(area.width.saturating_sub(4));
    let popup_height = 25.min(area.height.saturating_sub(4));

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

    // Split inner area into two columns
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    // Left column: Navigation, Overview, Logs, Accounts
    let left_text = vec![
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
    ];

    // Right column: Config, Mappings, Usage, General
    let right_text = vec![
        Line::from(Span::styled("Config Tab", theme::primary())),
        Line::from("  Enter         Edit field"),
        Line::from("  Space         Toggle boolean"),
        Line::from("  s             Save config"),
        Line::from("  r             Restart daemon"),
        Line::from(""),
        Line::from(Span::styled("Mappings Tab", theme::primary())),
        Line::from("  p             Cycle preset"),
        Line::from("  Enter         Edit pattern"),
        Line::from("  < >           Cycle target"),
        Line::from("  a             Add rule"),
        Line::from("  d             Delete rule"),
        Line::from("  b             Background model"),
        Line::from("  s             Save mappings"),
        Line::from(""),
        Line::from(Span::styled("Usage Tab", theme::primary())),
        Line::from("  r             Reset history"),
        Line::from(""),
        Line::from(Span::styled("General", theme::primary())),
        Line::from("  ?             Toggle help"),
        Line::from("  q / Esc       Quit"),
    ];

    frame.render_widget(Paragraph::new(left_text), columns[0]);
    frame.render_widget(Paragraph::new(right_text), columns[1]);
}
