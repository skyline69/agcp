use ratatui::prelude::*;
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, BorderType, Borders, Paragraph};

use crate::tui::app::App;
use crate::tui::theme;

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

    // Layout: top summary row + model bar chart
    let layout = Layout::vertical([
        Constraint::Length(5), // Summary panel
        Constraint::Fill(1),   // Model token chart
    ])
    .split(area);

    render_summary(frame, layout[0], stats);
    render_model_chart(frame, layout[1], stats);
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

    let line = Line::from(spans);
    // Add some vertical padding
    let text_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    frame.render_widget(Paragraph::new(line), text_area);
}

/// Render model-level token usage as a grouped bar chart
fn render_model_chart(frame: &mut Frame, area: Rect, stats: &crate::tui::data::TokenStats) {
    if stats.models.is_empty() {
        return;
    }

    let block = Block::default()
        .title(" Tokens by Model ")
        .title_style(theme::primary())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .style(Style::default().bg(theme::SURFACE));

    let inner = block.inner(area);

    // If there's not enough room for a bar chart, fall back to text
    if inner.height < 4 || inner.width < 20 {
        frame.render_widget(block, area);
        return;
    }

    // Build bar groups — one group per model with input/output bars
    let groups: Vec<BarGroup> = stats
        .models
        .iter()
        .map(|m| {
            let label = shorten_model_name(&m.model);
            BarGroup::default()
                .label(Line::from(label).centered())
                .bars(&[
                    Bar::default()
                        .value(m.input_tokens)
                        .label(Line::from(format_tokens_short(m.input_tokens)))
                        .style(Style::default().fg(theme::SECONDARY)),
                    Bar::default()
                        .value(m.output_tokens)
                        .label(Line::from(format_tokens_short(m.output_tokens)))
                        .style(Style::default().fg(theme::PRIMARY)),
                ])
        })
        .collect();

    // Calculate bar width based on available space and number of models
    let num_models = stats.models.len();
    // Each group has 2 bars + 1 gap between bars + 2 gap between groups
    // So width per group = bar_width * 2 + gap(1) + group_gap(2)
    let available_width = inner.width as usize;
    let bar_width = if num_models == 0 {
        5
    } else {
        // Each group takes: bar_width * 2 + 1 (inter-bar gap) + 2 (group gap)
        // Total = num_models * (2*bw + 3) - 2 (no trailing group gap)
        // Solve for bw: available_width + 2 = num_models * (2*bw + 3)
        let max_bw = ((available_width + 2) / num_models).saturating_sub(3) / 2;
        max_bw.clamp(3, 12)
    };

    let mut chart = BarChart::default()
        .block(block)
        .bar_width(bar_width as u16)
        .bar_gap(1)
        .group_gap(2)
        .bar_style(Style::default().fg(theme::DIM))
        .value_style(
            Style::default()
                .fg(theme::TEXT)
                .add_modifier(Modifier::BOLD),
        );

    for group in groups {
        chart = chart.data(group);
    }

    frame.render_widget(chart, area);

    // Legend at the bottom-right of the inner area
    let legend = Line::from(vec![
        Span::styled(" \u{2588} ", Style::default().fg(theme::SECONDARY)),
        Span::styled("Input ", theme::dim()),
        Span::styled(" \u{2588} ", Style::default().fg(theme::PRIMARY)),
        Span::styled("Output ", theme::dim()),
    ]);
    let legend_width = 22u16;
    if inner.width > legend_width + 2 && inner.height > 1 {
        let legend_area = Rect::new(
            inner.x + inner.width - legend_width - 1,
            inner.y,
            legend_width,
            1,
        );
        frame.render_widget(Paragraph::new(legend), legend_area);
    }
}

/// Format token count for display with appropriate suffix
fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else if count >= 1_000 {
        // Use comma separator for thousands
        format!("{},{:03}", count / 1000, count % 1000)
    } else {
        format!("{}", count)
    }
}

/// Shorter format for bar labels
fn format_tokens_short(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.0}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.0}K", count as f64 / 1_000.0)
    } else {
        format!("{}", count)
    }
}

/// Shorten model names for display in chart labels
fn shorten_model_name(name: &str) -> String {
    // Strip common prefixes/suffixes to make names more readable
    let name = name.replace("claude-", "").replace("gemini-", "gem-");

    // Truncate date suffixes like -20250514
    if let Some(idx) = name.rfind("-202") {
        name[..idx].to_string()
    } else {
        name
    }
}
