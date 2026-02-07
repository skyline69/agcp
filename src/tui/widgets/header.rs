use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::theme;

pub struct Header<'a> {
    pub server_running: bool,
    pub uptime: &'a str,
    /// Animation time in milliseconds
    pub time_ms: u64,
}

impl<'a> Header<'a> {
    pub fn new(server_running: bool, uptime: &'a str, time_ms: u64) -> Self {
        Self {
            server_running,
            uptime,
            time_ms,
        }
    }
}

impl Widget for Header<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Status style - pulse when running
        let status_style = if self.server_running {
            theme::pulse_success(self.time_ms)
        } else {
            theme::error()
        };

        let status_icon = if self.server_running { "●" } else { "○" };
        let status_text = if self.server_running {
            "Running"
        } else {
            "Stopped"
        };

        // Layout: [AGCP] ... [● Status] [Uptime]
        let chunks = Layout::horizontal([
            Constraint::Length(6),  // "AGCP"
            Constraint::Fill(1),    // Spacer
            Constraint::Length(12), // Status
            Constraint::Length(12), // Uptime
        ])
        .split(area);

        // App name with rainbow RGB effect
        let title = "AGCP";
        let rainbow_spans: Vec<Span> = title
            .chars()
            .enumerate()
            .map(|(i, c)| Span::styled(c.to_string(), theme::rainbow_style(self.time_ms, i)))
            .collect();
        Paragraph::new(Line::from(rainbow_spans)).render(chunks[0], buf);

        // Status
        let status = Line::from(vec![
            Span::styled(status_icon, status_style),
            Span::raw(" "),
            Span::styled(status_text, status_style),
        ]);
        Paragraph::new(status)
            .alignment(Alignment::Right)
            .render(chunks[2], buf);

        // Uptime
        Paragraph::new(self.uptime)
            .style(theme::dim())
            .alignment(Alignment::Right)
            .render(chunks[3], buf);
    }
}
