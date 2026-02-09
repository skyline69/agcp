use ratatui::prelude::*;
use ratatui::symbols::Marker;
use ratatui::widgets::{
    Axis, Block, BorderType, Borders, Chart, Dataset, GraphType, LegendPosition, Paragraph,
};

use crate::tui::app::App;
use crate::tui::theme;

/// Distinct colors for different models in the chart
const MODEL_COLORS: &[Color] = &[
    Color::Rgb(0, 212, 170),   // Cyan/Teal (PRIMARY)
    Color::Rgb(10, 132, 255),  // Blue (SECONDARY)
    Color::Rgb(248, 81, 73),   // Red (ERROR)
    Color::Rgb(210, 153, 34),  // Amber (WARNING)
    Color::Rgb(63, 185, 80),   // Green (SUCCESS)
    Color::Rgb(188, 140, 255), // Purple
    Color::Rgb(255, 166, 87),  // Orange
    Color::Rgb(255, 105, 180), // Pink
];

/// Render the usage tab content showing token consumption statistics
pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    // Fill background
    let bg_block = Block::default().style(Style::default().bg(theme::BACKGROUND));
    frame.render_widget(bg_block, area);

    let Some(ref stats) = app.cached_token_stats else {
        render_no_data(frame, area);
        return;
    };

    if stats.total_input_tokens == 0 && stats.total_output_tokens == 0 {
        render_no_data(frame, area);
        return;
    }

    // Layout: top summary row + time-series chart
    let layout = Layout::vertical([
        Constraint::Length(5), // Summary panel
        Constraint::Fill(1),   // Cumulative token chart
    ])
    .split(area);

    render_summary(frame, layout[0], stats);
    render_cumulative_chart(frame, layout[1], app);
}

/// Render when no token data is available
fn render_no_data(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" Token Usage ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(Style::default().bg(theme::SURFACE));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let msg = Paragraph::new("No token usage data yet. Make some API requests to see stats here.")
        .style(theme::dim())
        .alignment(Alignment::Center);

    // Center vertically
    let y_offset = inner.height / 2;
    let centered = Rect::new(inner.x, inner.y + y_offset, inner.width, 1);
    frame.render_widget(msg, centered);
}

/// Render the summary panel with total token counts
fn render_summary(frame: &mut Frame, area: Rect, stats: &crate::tui::data::TokenStats) {
    let block = Block::default()
        .title(" Token Usage Summary ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(Style::default().bg(theme::SURFACE));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let total = stats.total_input_tokens + stats.total_output_tokens;

    let mut spans = vec![
        Span::styled(
            "  Input: ",
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format_tokens(stats.total_input_tokens),
            Style::default().fg(theme::SECONDARY),
        ),
        Span::raw("    "),
        Span::styled(
            "Output: ",
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format_tokens(stats.total_output_tokens),
            Style::default().fg(theme::PRIMARY),
        ),
    ];

    if stats.total_cache_read_tokens > 0 {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            "Cached: ",
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format_tokens(stats.total_cache_read_tokens),
            Style::default().fg(theme::SUCCESS),
        ));
    }

    spans.push(Span::raw("    "));
    spans.push(Span::styled(
        "Total: ",
        Style::default()
            .fg(theme::TEXT)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format_tokens(total),
        Style::default().fg(theme::WARNING),
    ));

    // Second line: per-model totals with matching colors
    let mut model_spans = vec![Span::raw("  ")];
    for (i, m) in stats.models.iter().enumerate() {
        let color = MODEL_COLORS[i % MODEL_COLORS.len()];
        if i > 0 {
            model_spans.push(Span::raw("  "));
        }
        model_spans.push(Span::styled(
            shorten_model_name(&m.model),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        model_spans.push(Span::styled(
            format!(" {}", format_tokens(m.input_tokens + m.output_tokens)),
            Style::default().fg(color),
        ));
    }

    let lines = vec![Line::from(spans), Line::from(model_spans)];

    let text_area = Rect::new(inner.x, inner.y, inner.width, inner.height.min(3));
    frame.render_widget(Paragraph::new(lines), text_area);
}

/// Render the cumulative token usage chart with one line per model
fn render_cumulative_chart(frame: &mut Frame, area: Rect, app: &App) {
    let series = app.token_history.get_cumulative_series();

    if series.is_empty() {
        // Not enough data points yet
        let block = Block::default()
            .title(" Cumulative Token Usage ")
            .title_style(theme::primary())
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .style(Style::default().bg(theme::SURFACE));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let msg =
            Paragraph::new("Collecting data... chart will appear after the first poll interval.")
                .style(theme::dim());
        frame.render_widget(msg, inner);
        return;
    }

    // Find max Y across all series
    let max_y = series
        .iter()
        .flat_map(|(_, pts)| pts.iter().map(|(_, y)| *y))
        .fold(0.0f64, f64::max);

    // Time range in minutes
    let time_range = app.token_history.get_time_range_minutes();
    let x_bound = time_range.max(1.0);

    // Build datasets
    let datasets: Vec<Dataset> = series
        .iter()
        .enumerate()
        .map(|(i, (name, points))| {
            let color = MODEL_COLORS[i % MODEL_COLORS.len()];
            Dataset::default()
                .name(shorten_model_name(name))
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(color))
                .data(points)
        })
        .collect();

    // X axis: time in minutes
    let x_labels = build_time_labels(x_bound);
    let x_axis = Axis::default()
        .style(theme::dim())
        .bounds([0.0, x_bound])
        .labels(x_labels);

    // Y axis: cumulative tokens
    let y_bound = if max_y <= 100.0 {
        (max_y * 1.2).max(10.0)
    } else {
        max_y * 1.1
    };

    let y_labels = build_y_labels(y_bound);
    let y_axis = Axis::default()
        .title(Span::styled("tokens", theme::dim()))
        .style(theme::dim())
        .bounds([0.0, y_bound])
        .labels(y_labels);

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(" Cumulative Token Usage ")
                .title_style(theme::primary())
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme::border())
                .style(Style::default().bg(theme::SURFACE)),
        )
        .x_axis(x_axis)
        .y_axis(y_axis)
        .legend_position(Some(LegendPosition::TopRight))
        .style(Style::default().bg(theme::SURFACE));

    frame.render_widget(chart, area);
}

/// Build X-axis labels based on the time range
fn build_time_labels(total_minutes: f64) -> Vec<Span<'static>> {
    if total_minutes <= 5.0 {
        vec![
            Span::styled("0m", theme::dim()),
            Span::styled(format!("{:.0}m", total_minutes), theme::dim()),
        ]
    } else if total_minutes <= 60.0 {
        let mid = (total_minutes / 2.0).round();
        vec![
            Span::styled("0m", theme::dim()),
            Span::styled(format!("{:.0}m", mid), theme::dim()),
            Span::styled(format!("{:.0}m", total_minutes.round()), theme::dim()),
        ]
    } else if total_minutes <= 1440.0 {
        // Up to 24 hours — show in hours
        let hours = total_minutes / 60.0;
        let mid = (hours / 2.0).round();
        vec![
            Span::styled("0h", theme::dim()),
            Span::styled(format!("{:.0}h", mid), theme::dim()),
            Span::styled(format!("{:.0}h", hours.round()), theme::dim()),
        ]
    } else {
        // Multiple days
        let days = total_minutes / 1440.0;
        vec![
            Span::styled("0d", theme::dim()),
            Span::styled(format!("{:.1}d", days / 2.0), theme::dim()),
            Span::styled(format!("{:.1}d", days), theme::dim()),
        ]
    }
}

/// Build Y-axis labels with appropriate formatting
fn build_y_labels(y_bound: f64) -> Vec<Span<'static>> {
    if y_bound <= 10.0 {
        vec![
            Span::styled("0", theme::dim()),
            Span::styled(format!("{}", y_bound.ceil() as u64), theme::dim()),
        ]
    } else {
        let mid = (y_bound / 2.0).round() as u64;
        let max_label = y_bound.ceil() as u64;
        vec![
            Span::styled("0", theme::dim()),
            Span::styled(format_tokens_short(mid), theme::dim()),
            Span::styled(format_tokens_short(max_label), theme::dim()),
        ]
    }
}

/// Format token count for display with appropriate suffix
fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else if count >= 1_000 {
        format!("{},{:03}", count / 1000, count % 1000)
    } else {
        format!("{}", count)
    }
}

/// Shorter format for axis labels
fn format_tokens_short(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.0}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.0}K", count as f64 / 1_000.0)
    } else {
        format!("{}", count)
    }
}

/// Shorten model names for display in chart legend
fn shorten_model_name(name: &str) -> String {
    let name = name.replace("claude-", "").replace("gemini-", "gem-");

    if let Some(idx) = name.rfind("-202") {
        name[..idx].to_string()
    } else {
        name
    }
}
