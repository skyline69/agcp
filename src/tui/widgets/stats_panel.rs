//! Stats panel widget

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::tui::theme;

pub struct StatsPanel {
    pub total_requests: u64,
    pub requests_per_min: f64,
    pub avg_response_ms: Option<u64>,
    pub active_accounts: usize,
    pub total_accounts: usize,
    pub quota_remaining: Option<f64>,
}

impl StatsPanel {
    pub fn new(
        total_requests: u64,
        requests_per_min: f64,
        avg_response_ms: Option<u64>,
        active_accounts: usize,
        total_accounts: usize,
        quota_remaining: Option<f64>,
    ) -> Self {
        Self {
            total_requests,
            requests_per_min,
            avg_response_ms,
            active_accounts,
            total_accounts,
            quota_remaining,
        }
    }
}

impl Widget for StatsPanel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" Quick Stats ")
            .title_style(theme::primary())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .style(theme::surface());

        let inner = block.inner(area);
        block.render(area, buf);

        let mut lines = vec![
            // Row 1: Total requests and rate
            Line::from(vec![
                Span::styled("Requests ", theme::dim()),
                Span::styled(format_number(self.total_requests), theme::primary()),
                Span::styled("  Rate ", theme::dim()),
                Span::styled(format!("{:.1}", self.requests_per_min), theme::success()),
                Span::styled("/min", theme::dim()),
            ]),
            // Row 2: Average response time
            Line::from(vec![
                Span::styled("Avg Time ", theme::dim()),
                if let Some(ms) = self.avg_response_ms {
                    let style = if ms < 1000 {
                        theme::success()
                    } else if ms < 5000 {
                        theme::warning()
                    } else {
                        theme::error()
                    };
                    Span::styled(format_duration(ms), style)
                } else {
                    Span::styled("--", theme::dim())
                },
            ]),
            // Row 3: Accounts and quota
            Line::from(vec![
                Span::styled("Accounts ", theme::dim()),
                Span::styled(
                    format!("{}/{}", self.active_accounts, self.total_accounts),
                    theme::primary(),
                ),
                Span::styled("  Quota ", theme::dim()),
                if let Some(quota) = self.quota_remaining {
                    let pct = (quota * 100.0).round() as u32;
                    let style = if quota <= 0.1 {
                        theme::error()
                    } else if quota <= 0.3 {
                        theme::warning()
                    } else {
                        theme::success()
                    };
                    Span::styled(format!("{}%", pct), style)
                } else {
                    Span::styled("--%", theme::dim())
                },
            ]),
        ];

        // Ensure we don't overflow the available height
        lines.truncate(inner.height as usize);

        Paragraph::new(lines).render(inner, buf);
    }
}

/// Format a number with thousands separators
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.insert(0, ',');
        }
        result.insert(0, c);
    }
    result
}

/// Format duration in ms to a human-readable string
fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let secs = ms / 1000;
        format!("{}m{}s", secs / 60, secs % 60)
    }
}
