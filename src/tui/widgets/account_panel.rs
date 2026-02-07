//! Account panel widget

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::cloudcode::quota::ModelQuota;
use crate::tui::data::AccountInfo;
use crate::tui::theme;

pub struct AccountPanel<'a> {
    pub accounts: &'a [AccountInfo],
    pub quota_data: &'a [ModelQuota],
}

impl<'a> AccountPanel<'a> {
    pub fn new(accounts: &'a [AccountInfo], quota_data: &'a [ModelQuota]) -> Self {
        Self {
            accounts,
            quota_data,
        }
    }

    /// Calculate average quota from live quota data
    fn get_live_quota_fraction(&self) -> f64 {
        if self.quota_data.is_empty() {
            return 1.0; // Default to 100% if no data
        }

        // Calculate average remaining quota across all models
        let total: f64 = self.quota_data.iter().map(|q| q.remaining_fraction).sum();
        total / self.quota_data.len() as f64
    }
}

impl Widget for AccountPanel<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default()
            .title(" Active Account ")
            .title_style(theme::primary())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .style(theme::surface());

        let inner = block.inner(area);
        block.render(area, buf);

        let active = self.accounts.iter().find(|a| a.is_active);

        let lines = if let Some(account) = active {
            // Use live quota data if available, otherwise fall back to stored quota
            let quota_fraction = if !self.quota_data.is_empty() {
                self.get_live_quota_fraction()
            } else {
                account.quota_fraction
            };

            let quota_bar = render_quota_bar(quota_fraction, 10);
            vec![
                Line::from(vec![Span::styled(&account.email, theme::primary())]),
                Line::from(vec![
                    Span::raw("Quota: "),
                    Span::styled(quota_bar, quota_color(quota_fraction)),
                    Span::styled(format!(" {:.0}%", quota_fraction * 100.0), theme::dim()),
                ]),
            ]
        } else {
            vec![Line::from(Span::styled("No active account", theme::dim()))]
        };

        Paragraph::new(lines).render(inner, buf);
    }
}

/// Render a progress bar for quota
fn render_quota_bar(fraction: f64, width: usize) -> String {
    let filled = (fraction * width as f64).round() as usize;
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Get color based on quota level
fn quota_color(fraction: f64) -> Style {
    // Note: For quota, LOW remaining = bad (red), HIGH remaining = good (green)
    if fraction <= 0.1 {
        theme::error()
    } else if fraction <= 0.3 {
        theme::warning()
    } else {
        theme::success()
    }
}
