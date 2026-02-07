use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::cloudcode::quota::ModelQuota;
use crate::models::get_model_family;
use crate::tui::theme;
use crate::tui::widgets::QuotaDonut;

/// Render the quota view with donut charts and visual bars for each model
pub fn render(frame: &mut Frame, area: Rect, quotas: &[ModelQuota]) {
    let block = Block::default()
        .title(" Model Quotas ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if quotas.is_empty() {
        let msg = Text::from("Loading quota data...\n\nQuota will appear shortly after startup.")
            .style(theme::dim())
            .centered();
        frame.render_widget(msg, inner);
        return;
    }

    // Group by family (Claude vs Gemini)
    let mut claude_quotas: Vec<&ModelQuota> = Vec::new();
    let mut gemini_quotas: Vec<&ModelQuota> = Vec::new();

    for q in quotas {
        match get_model_family(&q.model_id) {
            "claude" => claude_quotas.push(q),
            "gemini" => gemini_quotas.push(q),
            _ => {}
        }
    }

    // Sort each group by remaining quota (ascending - lowest first)
    claude_quotas.sort_by(|a, b| {
        a.remaining_fraction
            .partial_cmp(&b.remaining_fraction)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    gemini_quotas.sort_by(|a, b| {
        a.remaining_fraction
            .partial_cmp(&b.remaining_fraction)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Calculate average quota for each family
    let claude_avg = if claude_quotas.is_empty() {
        1.0
    } else {
        claude_quotas
            .iter()
            .map(|q| q.remaining_fraction)
            .sum::<f64>()
            / claude_quotas.len() as f64
    };
    let gemini_avg = if gemini_quotas.is_empty() {
        1.0
    } else {
        gemini_quotas
            .iter()
            .map(|q| q.remaining_fraction)
            .sum::<f64>()
            / gemini_quotas.len() as f64
    };

    // Layout: donuts on left, details on right
    let donut_width = 26u16; // Space for two donuts side by side
    let chunks =
        Layout::horizontal([Constraint::Length(donut_width), Constraint::Min(30)]).split(inner);

    // Render donut charts on the left
    render_donuts(frame, chunks[0], claude_avg, gemini_avg);

    // Render detailed list on the right
    render_detail_list(frame, chunks[1], &claude_quotas, &gemini_quotas);
}

/// Render the donut charts for Claude and Gemini
fn render_donuts(frame: &mut Frame, area: Rect, claude_avg: f64, gemini_avg: f64) {
    // Stack two donuts vertically
    let chunks = Layout::vertical([
        Constraint::Length(1), // Top spacing
        Constraint::Length(9), // Claude donut
        Constraint::Length(1), // Gap between donut and label
        Constraint::Length(1), // Claude label
        Constraint::Length(2), // Spacing between sections
        Constraint::Length(9), // Gemini donut
        Constraint::Length(1), // Gap between donut and label
        Constraint::Length(1), // Gemini label
        Constraint::Min(0),    // Remaining space
    ])
    .split(area);

    // Claude donut (no internal label)
    frame.render_widget(QuotaDonut::new(claude_avg), chunks[1]);

    // Claude label with percentage below donut
    let claude_pct = (claude_avg * 100.0).round() as u32;
    let claude_label = Paragraph::new(Line::from(vec![
        Span::styled("Claude ", theme::primary().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}%", claude_pct), theme::dim()),
    ]))
    .centered();
    frame.render_widget(claude_label, chunks[3]);

    // Gemini donut (no internal label)
    frame.render_widget(QuotaDonut::new(gemini_avg), chunks[5]);

    // Gemini label with percentage below donut
    let gemini_pct = (gemini_avg * 100.0).round() as u32;
    let gemini_label = Paragraph::new(Line::from(vec![
        Span::styled("Gemini ", theme::primary().add_modifier(Modifier::BOLD)),
        Span::styled(format!("{}%", gemini_pct), theme::dim()),
    ]))
    .centered();
    frame.render_widget(gemini_label, chunks[7]);
}

/// Render the detailed quota list
fn render_detail_list(
    frame: &mut Frame,
    area: Rect,
    claude_quotas: &[&ModelQuota],
    gemini_quotas: &[&ModelQuota],
) {
    let max_model_len = 25;
    let bar_width = area.width.saturating_sub(max_model_len as u16 + 18) as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Render Claude section
    if !claude_quotas.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "Claude Models",
            theme::primary().add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));

        for q in claude_quotas {
            lines.push(render_quota_line(q, max_model_len, bar_width));
        }
        lines.push(Line::from(""));
    }

    // Render Gemini section
    if !gemini_quotas.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "Gemini Models",
            theme::primary().add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));

        for q in gemini_quotas {
            lines.push(render_quota_line(q, max_model_len, bar_width));
        }
    }

    frame.render_widget(Paragraph::new(lines), area);
}

/// Render a single quota line
fn render_quota_line(quota: &ModelQuota, max_model_len: usize, bar_width: usize) -> Line<'static> {
    let bar = render_quota_bar(quota.remaining_fraction, bar_width);
    let style = quota_color(quota.remaining_fraction);
    let pct = (quota.remaining_fraction * 100.0).round() as u32;

    // Format reset time if available
    let reset_info = quota
        .reset_time
        .as_ref()
        .map(|t| format!(" ({})", format_reset_time(t)))
        .unwrap_or_default();

    Line::from(vec![
        Span::styled(
            format!(
                "{:<width$}",
                truncate_model_name(&quota.model_id, max_model_len),
                width = max_model_len
            ),
            Style::default().fg(theme::TEXT),
        ),
        Span::raw(" "),
        Span::styled(bar, style),
        Span::raw(" "),
        Span::styled(
            format!("{:>3}%", pct),
            if quota.remaining_fraction <= 0.1 {
                theme::error()
            } else {
                theme::dim()
            },
        ),
        Span::styled(reset_info, theme::dim()),
    ])
}

/// Truncate model name for display
fn truncate_model_name(model: &str, max: usize) -> String {
    // Remove common prefixes to save space
    let name = model
        .trim_start_matches("claude-")
        .trim_start_matches("gemini-");

    if name.len() <= max {
        name.to_string()
    } else {
        format!("{}...", &name[..max.saturating_sub(3)])
    }
}

/// Render a progress bar for quota
fn render_quota_bar(fraction: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
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

/// Format reset time as relative duration
fn format_reset_time(reset_time: &str) -> String {
    // Try to parse ISO 8601 timestamp and show relative time
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(reset_time) {
        let now = chrono::Utc::now();
        let duration = parsed.signed_duration_since(now);

        if duration.num_seconds() <= 0 {
            return "now".to_string();
        }

        let hours = duration.num_hours();
        let mins = duration.num_minutes() % 60;

        if hours > 0 {
            format!("{}h{}m", hours, mins)
        } else {
            format!("{}m", mins)
        }
    } else {
        reset_time.to_string()
    }
}
