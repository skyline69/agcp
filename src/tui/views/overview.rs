use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::{Axis, Block, BorderType, Borders, Chart, Dataset, GraphType, Paragraph};

use crate::tui::app::App;
use crate::tui::theme;
use crate::tui::widgets::{AccountPanel, StatsPanel, StatusPanel};

/// Render the overview tab content
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let status = app.get_cached_server_status();
    let accounts = &app.accounts;

    // Get config for address/port, but prefer the actual daemon address if available
    let (display_host, display_port) = crate::config::get_daemon_host_port();

    // Use cached stats (refreshed alongside logs every 500ms, not per frame)
    let log_request_count = app.cached_request_count;
    let log_models = &app.cached_model_usage;
    let rate_history = &app.cached_rate_history;
    let avg_response_ms = app.cached_avg_response_ms;
    let requests_per_min = app.cached_requests_per_min;

    // Calculate account stats
    let active_accounts = accounts
        .iter()
        .filter(|a| a.enabled && !a.is_invalid)
        .count();
    let total_accounts = accounts.len();

    // Calculate average quota from live data (across all accounts)
    let quota_remaining = app.get_overall_quota_fraction();

    // Use cached uptime string (refreshed every 500ms, not per frame)
    let uptime = &app.cached_uptime;

    // Layout: 4 rows
    // Row 1: Status | Quick Stats
    // Row 2: Request Rate Chart (line graph with axes)
    // Row 3: Active Account | Model Usage
    // Row 4: Recent Activity
    let main_chunks = Layout::vertical([
        Constraint::Length(7),  // Status + Stats row (5 lines + borders)
        Constraint::Length(10), // Request Rate Chart (needs more height for axes)
        Constraint::Length(5),  // Account + Model Usage
        Constraint::Fill(1),    // Recent Activity
    ])
    .split(area);

    // Top row: Status | Stats
    let top_chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_chunks[0]);

    // Status panel - use daemon uptime from logs
    let status_panel = StatusPanel::new(
        status,
        &display_host,
        display_port,
        uptime,
        app.daemon_status_message.as_ref(),
    );
    frame.render_widget(status_panel, top_chunks[0]);

    // Stats panel - comprehensive stats
    let (total_in, total_out) = (app.animated_input_tokens, app.animated_output_tokens);
    let stats_panel = StatsPanel::new(
        log_request_count,
        requests_per_min,
        avg_response_ms,
        active_accounts,
        total_accounts,
        quota_remaining,
        total_in,
        total_out,
    );
    frame.render_widget(stats_panel, top_chunks[1]);

    // Request rate graph - use log-based rate history
    render_rate_graph_from_history(frame, main_chunks[1], rate_history);

    // Middle row: Account | Model Usage
    let mid_chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(main_chunks[2]);

    // Account panel - pass active account's live quota data
    let account_panel = AccountPanel::new(accounts, app.get_active_quota_data());
    frame.render_widget(account_panel, mid_chunks[0]);

    // Model Usage panel - uses model info parsed from logs
    render_model_usage(frame, mid_chunks[1], log_models, log_request_count);

    // Bottom: Recent Activity
    render_recent_activity(frame, main_chunks[3], app);
}

/// Render request rate chart from log-based rate history
fn render_rate_graph_from_history(frame: &mut Frame, area: Rect, rate_history: &[u64]) {
    // First, fill the background to prevent transparency
    let bg_block = Block::default().style(Style::default().bg(theme::BACKGROUND));
    frame.render_widget(bg_block, area);

    if rate_history.is_empty() || rate_history.iter().all(|&x| x == 0) {
        let block = Block::default()
            .title(" Request Rate (last 60s) ")
            .title_style(theme::primary())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .style(Style::default().bg(theme::SURFACE));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let msg = Paragraph::new("No recent requests").style(theme::dim());
        frame.render_widget(msg, inner);
        return;
    }

    // Convert rate history to (x, y) data points for the chart
    // x = seconds ago (0 = oldest, 59 = most recent)
    // y = requests per second
    let data: Vec<(f64, f64)> = rate_history
        .iter()
        .enumerate()
        .map(|(i, &count)| (i as f64, count as f64))
        .collect();

    let max_val = rate_history.iter().max().copied().unwrap_or(1).max(1) as f64;

    // Create the dataset with a line graph
    let dataset = Dataset::default()
        .name("req/s")
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(theme::primary())
        .data(&data);

    // Create X axis (time: 60s ago to now)
    let x_axis = Axis::default()
        .style(theme::dim())
        .bounds([0.0, 60.0])
        .labels(vec![
            Span::styled("-60s", theme::dim()),
            Span::styled("-30s", theme::dim()),
            Span::styled("now", theme::dim()),
        ]);

    // Create Y axis with smart labels based on max value
    // Avoid duplicate labels like "1 1" when max is small
    let y_bound = if max_val <= 1.0 { 2.0 } else { max_val * 1.1 };
    let y_labels = if max_val <= 1.0 {
        vec![
            Span::styled("0", theme::dim()),
            Span::styled("1", theme::dim()),
            Span::styled("2", theme::dim()),
        ]
    } else if max_val <= 5.0 {
        // For small values, just show 0 and max
        vec![
            Span::styled("0", theme::dim()),
            Span::styled(format!("{}", max_val.ceil() as u64), theme::dim()),
        ]
    } else {
        // For larger values, show 0, mid, max
        let mid = (max_val / 2.0).round() as u64;
        let max_label = max_val.ceil() as u64;
        vec![
            Span::styled("0", theme::dim()),
            Span::styled(format!("{}", mid), theme::dim()),
            Span::styled(format!("{}", max_label), theme::dim()),
        ]
    };

    let y_axis = Axis::default()
        .style(theme::dim())
        .bounds([0.0, y_bound])
        .labels(y_labels);

    // Create the chart with solid background
    let chart = Chart::new(vec![dataset])
        .block(
            Block::default()
                .title(" Request Rate (last 60s) ")
                .title_style(theme::primary())
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border())
                .style(Style::default().bg(theme::SURFACE)),
        )
        .x_axis(x_axis)
        .y_axis(y_axis)
        .style(Style::default().bg(theme::SURFACE));

    frame.render_widget(chart, area);
}

/// Render model usage with horizontal bar chart
fn render_model_usage(
    frame: &mut Frame,
    area: Rect,
    models: &[crate::tui::data::ModelUsage],
    total: u64,
) {
    let block = Block::default()
        .title(" Model Usage ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if models.is_empty() || total == 0 {
        let msg = Paragraph::new("No requests yet").style(theme::dim());
        frame.render_widget(msg, inner);
        return;
    }

    // Build lines with bar chart
    let max_model_len = 20;
    let bar_width = inner.width.saturating_sub(max_model_len as u16 + 10) as usize;

    let lines: Vec<Line> = models
        .iter()
        .take(inner.height as usize)
        .map(|m| {
            let pct = (m.requests as f64 / total as f64 * 100.0).min(100.0);
            let filled = (pct / 100.0 * bar_width as f64) as usize;
            let bar = format!("{}{}", "█".repeat(filled), "░".repeat(bar_width - filled));
            let model_name = truncate(&m.model, max_model_len);

            Line::from(vec![
                Span::styled(
                    format!("{:<width$}", model_name, width = max_model_len),
                    theme::primary(),
                ),
                Span::raw(" "),
                Span::styled(bar, bar_color(pct)),
                Span::styled(format!(" {:>3.0}%", pct), theme::dim()),
            ])
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

/// Render recent activity from logs with scroll support
fn render_recent_activity(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .title(" Recent Activity ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(theme::surface());

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Store area for mouse scroll detection
    app.activity_area = area;

    if app.logs.is_empty() {
        let msg = Paragraph::new("No activity yet").style(theme::dim());
        frame.render_widget(msg, inner);
        return;
    }

    // Calculate visible lines and scroll offset
    let visible = inner.height as usize;
    let total_lines = app.logs.len();

    // activity_scroll is lines from bottom (0 = at bottom/newest)
    // We need to calculate where to start in the log buffer
    let scroll_offset = total_lines
        .saturating_sub(visible)
        .saturating_sub(app.activity_scroll);

    let lines: Vec<Line> = app
        .logs
        .iter()
        .skip(scroll_offset)
        .take(visible)
        .map(|entry| {
            // Use syntax highlighting from logs module
            super::logs::highlight_log_line(&entry.line, entry.level)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}

fn bar_color(pct: f64) -> Style {
    if pct >= 50.0 {
        theme::primary()
    } else if pct >= 25.0 {
        theme::success()
    } else {
        theme::dim()
    }
}
