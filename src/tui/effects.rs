//! Effects module for TUI animations using tachyonfx
//! Currently minimal - RGB effects are handled directly in widgets

use ratatui::layout::Rect;
use tachyonfx::Effect;

/// Effect keys for unique effect management
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum EffectKey {
    #[default]
    Startup,
    TabTransition,
    HelpOverlay,
}

/// Placeholder - no transition effects currently used
pub fn startup_sweep(_area: Rect) -> Effect {
    tachyonfx::fx::consume_tick()
}

/// Placeholder - no transition effects currently used  
pub fn tab_appear(_area: Rect) -> Effect {
    tachyonfx::fx::consume_tick()
}

/// Placeholder - no transition effects currently used
pub fn help_fade_in(_area: Rect) -> Effect {
    tachyonfx::fx::consume_tick()
}
