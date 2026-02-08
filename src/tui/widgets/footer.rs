use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::theme;

pub struct Footer {
    pub keybinds: Vec<(&'static str, &'static str)>,
}

impl Footer {
    pub fn new(keybinds: Vec<(&'static str, &'static str)>) -> Self {
        Self { keybinds }
    }

    /// Get context-sensitive keybinds for a specific tab
    pub fn for_tab(tab: crate::tui::app::Tab) -> Self {
        use crate::tui::app::Tab;

        let mut binds: Vec<(&'static str, &'static str)> =
            vec![("q", "Quit"), ("Tab", "Switch"), ("?", "Help")];

        match tab {
            Tab::Accounts => {
                binds.insert(2, ("c", "Clear"));
                binds.insert(2, ("s", "Sort"));
                binds.insert(2, ("/", "Search"));
                binds.insert(2, ("r", "Refresh"));
                binds.insert(2, ("e", "Toggle"));
                binds.insert(2, ("Enter", "Activate"));
            }
            Tab::Logs => {
                binds.insert(2, ("c", "Clear"));
                binds.insert(2, ("a", "Account"));
                binds.insert(2, ("/", "Search"));
                binds.insert(2, ("d/i/w/e", "Levels"));
            }
            Tab::Config => {
                return Self::new(vec![
                    ("↑/↓", "Navigate"),
                    ("Enter", "Edit"),
                    ("Space", "Toggle"),
                    ("◀/▶", "Cycle"),
                    ("s", "Save"),
                    ("r", "Restart"),
                    ("?", "Help"),
                ]);
            }
            Tab::Mappings => {
                return Self::new(vec![
                    ("↑/↓", "Navigate"),
                    ("p", "Preset"),
                    ("Enter", "Edit"),
                    ("◀/▶", "Target"),
                    ("a", "Add"),
                    ("d", "Delete"),
                    ("b", "Background"),
                    ("s", "Save"),
                    ("?", "Help"),
                ]);
            }
            _ => {}
        }

        Self::new(binds)
    }
}

impl Widget for Footer {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let spans: Vec<Span> = self
            .keybinds
            .iter()
            .enumerate()
            .flat_map(|(i, (key, desc))| {
                let mut s = vec![
                    Span::styled(*key, theme::primary()),
                    Span::raw(" "),
                    Span::styled(*desc, theme::dim()),
                ];
                if i < self.keybinds.len() - 1 {
                    s.push(Span::raw("  "));
                }
                s
            })
            .collect();

        Paragraph::new(Line::from(spans)).render(area, buf);
    }
}
