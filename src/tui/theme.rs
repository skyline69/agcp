// Theme module - some colors/styles reserved for future views
#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};

// Cyberpunk/Cyan color palette
pub const PRIMARY: Color = Color::Rgb(0, 212, 170); // #00D4AA - Cyan/Teal
pub const SECONDARY: Color = Color::Rgb(10, 132, 255); // #0A84FF - Blue
pub const BACKGROUND: Color = Color::Rgb(13, 17, 23); // #0D1117 - Dark
pub const SURFACE: Color = Color::Rgb(22, 27, 34); // #161B22 - Lighter
pub const TEXT: Color = Color::Rgb(230, 237, 243); // #E6EDF3 - Off-white
pub const DIM: Color = Color::Rgb(125, 133, 144); // #7D8590 - Gray
pub const SUCCESS: Color = Color::Rgb(63, 185, 80); // #3FB950 - Green
pub const WARNING: Color = Color::Rgb(210, 153, 34); // #D29922 - Amber
pub const ERROR: Color = Color::Rgb(248, 81, 73); // #F85149 - Red

/// Generate a rainbow color based on hue (0.0 - 1.0)
/// Uses HSV to RGB conversion with full saturation and value
pub fn rainbow(hue: f32) -> Color {
    let h = (hue % 1.0) * 6.0;
    let c = 1.0_f32;
    let x = 1.0 - (h % 2.0 - 1.0).abs();

    let (r, g, b) = match h as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    Color::Rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}

/// Generate rainbow color from elapsed time in milliseconds
/// Each character can have an offset for wave effect
pub fn rainbow_wave(time_ms: u64, char_offset: usize) -> Color {
    // Cycle through colors over ~3 seconds (3000ms)
    let hue = ((time_ms as f32 / 3000.0) + (char_offset as f32 * 0.1)) % 1.0;
    rainbow(hue)
}

/// Get rainbow style for a character in animated text
pub fn rainbow_style(time_ms: u64, char_index: usize) -> Style {
    Style::default()
        .fg(rainbow_wave(time_ms, char_index))
        .add_modifier(Modifier::BOLD)
}

/// Base style with background color
pub fn base() -> Style {
    Style::default().bg(BACKGROUND).fg(TEXT)
}

/// Style for panel/card surfaces
pub fn surface() -> Style {
    Style::default().bg(SURFACE).fg(TEXT)
}

/// Style for primary accent (selected, active)
pub fn primary() -> Style {
    Style::default().fg(PRIMARY)
}

/// Style for dimmed/secondary text
pub fn dim() -> Style {
    Style::default().fg(DIM)
}

/// Style for success indicators
pub fn success() -> Style {
    Style::default().fg(SUCCESS)
}

/// Style for warning indicators
pub fn warning() -> Style {
    Style::default().fg(WARNING)
}

/// Style for error indicators
pub fn error() -> Style {
    Style::default().fg(ERROR)
}

/// Style for selected/highlighted items
pub fn selected() -> Style {
    Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)
}

/// Style for borders
pub fn border() -> Style {
    Style::default().fg(DIM)
}

/// Style for focused/active borders
pub fn border_focused() -> Style {
    Style::default().fg(PRIMARY)
}

/// Generate a pulsing/breathing brightness value (0.0 - 1.0)
/// Uses a smooth sine wave for natural-looking animation
pub fn pulse(time_ms: u64, period_ms: u64) -> f32 {
    let t = (time_ms % period_ms) as f32 / period_ms as f32;
    // Sine wave from 0.4 to 1.0 (never fully dim)
    0.4 + 0.6 * (t * std::f32::consts::PI * 2.0).sin().abs()
}

/// Interpolate between two colors based on factor (0.0 - 1.0)
fn lerp_color(from: Color, to: Color, factor: f32) -> Color {
    match (from, to) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let r = (r1 as f32 + (r2 as f32 - r1 as f32) * factor) as u8;
            let g = (g1 as f32 + (g2 as f32 - g1 as f32) * factor) as u8;
            let b = (b1 as f32 + (b2 as f32 - b1 as f32) * factor) as u8;
            Color::Rgb(r, g, b)
        }
        _ => to,
    }
}

/// Get a pulsing style for "Running" status indicator
/// Smoothly fades between bright green and a dimmer green
pub fn pulse_success(time_ms: u64) -> Style {
    let factor = pulse(time_ms, 2000); // 2 second cycle
    let dim_green = Color::Rgb(30, 90, 40); // Dimmer green
    let color = lerp_color(dim_green, SUCCESS, factor);
    Style::default().fg(color)
}
