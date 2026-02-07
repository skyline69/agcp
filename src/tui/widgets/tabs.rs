use ratatui::prelude::*;
use ratatui::widgets::Tabs as RataTabs;

use crate::tui::app::Tab;
use crate::tui::theme;

pub struct TabBar {
    pub current: Tab,
    pub hovered: Option<usize>,
}

impl TabBar {
    pub fn new(current: Tab) -> Self {
        Self {
            current,
            hovered: None,
        }
    }

    pub fn hovered(mut self, hovered: Option<usize>) -> Self {
        self.hovered = hovered;
        self
    }
}

impl Widget for TabBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let titles: Vec<Line> = Tab::all()
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let style = if *t == self.current {
                    theme::selected()
                } else if self.hovered == Some(i) {
                    // Hover style: brighter than dim but not as bright as selected
                    Style::default()
                        .fg(theme::TEXT)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    theme::dim()
                };
                Line::from(t.name()).style(style)
            })
            .collect();

        let tabs = RataTabs::new(titles)
            .select(
                Tab::all()
                    .iter()
                    .position(|t| *t == self.current)
                    .unwrap_or(0),
            )
            .highlight_style(theme::selected())
            .divider("│")
            .padding(" ", " ");

        tabs.render(area, buf);
    }
}
