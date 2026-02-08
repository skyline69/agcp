//! Status panel widget

use std::time::{Duration, Instant};

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::data::ServerStatus;
use crate::tui::theme;

/// Duration after which the daemon status message auto-clears
const MESSAGE_TTL: Duration = Duration::from_secs(4);

pub struct StatusPanel<'a> {
    pub status: ServerStatus,
    pub address: &'a str,
    pub port: u16,
    pub uptime: &'a str,
    pub message: Option<&'a (String, bool, Instant)>,
}

impl<'a> StatusPanel<'a> {
    pub fn new(
        status: ServerStatus,
        address: &'a str,
        port: u16,
        uptime: &'a str,
        message: Option<&'a (String, bool, Instant)>,
    ) -> Self {
        Self {
            status,
            address,
            port,
            uptime,
            message,
        }
    }
}

impl Widget for StatusPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (status_icon, status_text, status_style) = match self.status {
            ServerStatus::Running => ("●", "Running", theme::success()),
            ServerStatus::Stopped => ("○", "Stopped", theme::error()),
        };

        let block = Block::default()
            .title(" Status ")
            .title_style(theme::primary())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .style(theme::surface());

        let inner = block.inner(area);
        block.render(area, buf);

        // Content lines
        let mut lines = vec![
            Line::from(vec![
                Span::styled(status_icon, status_style),
                Span::raw(" Server: "),
                Span::styled(status_text, status_style),
            ]),
            Line::from(vec![
                Span::styled("●", theme::dim()),
                Span::raw(" Address: "),
                Span::styled(self.address, theme::primary()),
            ]),
            Line::from(vec![
                Span::styled("●", theme::dim()),
                Span::raw(" Port: "),
                Span::styled(self.port.to_string(), theme::primary()),
            ]),
            Line::from(vec![
                Span::styled("●", theme::dim()),
                Span::raw(" Uptime: "),
                Span::styled(self.uptime, theme::dim()),
            ]),
        ];

        // Show transient action message (if within TTL)
        if let Some((msg, is_error, created)) = self.message
            && created.elapsed() < MESSAGE_TTL
        {
            let style = if *is_error {
                theme::error()
            } else {
                theme::success()
            };
            lines.push(Line::from(Span::styled(format!("  ▸ {}", msg), style)));
        }

        Paragraph::new(lines).render(inner, buf);
    }
}
