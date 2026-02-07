//! ASCII donut chart widget for displaying quota/usage data
//!
//! Uses half-block characters (▀▄) for smoother circular rendering.

use ratatui::prelude::*;
use ratatui::widgets::Widget;

use std::f64::consts::PI;

/// A single slice of the donut chart
#[derive(Clone)]
pub struct DonutSlice {
    pub value: f64,
    pub color: Color,
}

/// ASCII donut chart widget with smooth rendering
pub struct DonutChart {
    slices: Vec<DonutSlice>,
    radius: f64,
    thickness: f64,
}

impl DonutChart {
    pub fn new(slices: Vec<DonutSlice>) -> Self {
        Self {
            slices,
            radius: 4.0,
            thickness: 1.5,
        }
    }

    pub fn radius(mut self, radius: f64) -> Self {
        self.radius = radius;
        self
    }

    pub fn thickness(mut self, thickness: f64) -> Self {
        self.thickness = thickness;
        self
    }

    /// Check if a point is inside the donut ring
    fn is_in_ring(&self, x: f64, y: f64) -> bool {
        let dist = (x * x + y * y).sqrt();
        let inner = self.radius - self.thickness;
        dist >= inner && dist <= self.radius
    }

    /// Get the slice color for a given angle
    fn get_slice_color(&self, angle: f64, angles: &[(f64, f64, Color)]) -> Option<Color> {
        for (start, end, color) in angles {
            let mut a = angle;
            let s = *start;
            let mut e = *end;

            // Normalize angles
            while a < s {
                a += 2.0 * PI;
            }
            while e < s {
                e += 2.0 * PI;
            }

            if a >= s && a < e {
                return Some(*color);
            }
        }
        None
    }
}

impl Widget for DonutChart {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width < 6 || area.height < 4 {
            return;
        }

        let total: f64 = self.slices.iter().map(|s| s.value).sum();
        if total <= 0.0 {
            return;
        }

        // Center of the chart
        let center_x = area.width as f64 / 2.0;
        let center_y = area.height as f64 / 2.0;

        // Terminal characters are roughly 2:1 (height:width), so we compensate
        let aspect_ratio = 2.0;

        // Build angle ranges for each slice (start at top, go clockwise)
        let mut angles: Vec<(f64, f64, Color)> = Vec::new();
        let mut current_angle = -PI / 2.0; // Start at top (12 o'clock)

        for slice in &self.slices {
            let sweep = (slice.value / total) * 2.0 * PI;
            angles.push((current_angle, current_angle + sweep, slice.color));
            current_angle += sweep;
        }

        // Render using half-block technique for smoother circles
        for row in 0..area.height {
            for col in 0..area.width {
                let x = area.x + col;
                let y = area.y + row;

                // Map to centered coordinates, accounting for aspect ratio
                let cx = (col as f64 - center_x) / aspect_ratio;

                // We check two sub-pixels per cell (top half and bottom half)
                let cy_top = row as f64 - center_y - 0.25;
                let cy_bot = row as f64 - center_y + 0.25;

                let in_top = self.is_in_ring(cx, cy_top);
                let in_bot = self.is_in_ring(cx, cy_bot);

                if in_top || in_bot {
                    // Get the angle for coloring (use center of cell)
                    let cy = row as f64 - center_y;
                    let angle = cy.atan2(cx);

                    if let Some(color) = self.get_slice_color(angle, &angles) {
                        let (ch, style) = match (in_top, in_bot) {
                            (true, true) => ("█", Style::default().fg(color)),
                            (true, false) => ("▀", Style::default().fg(color)),
                            (false, true) => ("▄", Style::default().fg(color)),
                            (false, false) => continue,
                        };
                        buf.set_string(x, y, ch, style);
                    }
                }
            }
        }
    }
}

/// Simple filled/remaining donut for showing a single value (like quota remaining)
pub struct QuotaDonut {
    remaining: f64,
}

impl QuotaDonut {
    pub fn new(remaining_fraction: f64) -> Self {
        Self {
            remaining: remaining_fraction.clamp(0.0, 1.0),
        }
    }
}

impl Widget for QuotaDonut {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Color based on remaining quota
        let remaining_color = if self.remaining <= 0.1 {
            Color::Red
        } else if self.remaining <= 0.3 {
            Color::Yellow
        } else {
            Color::Green
        };

        // Create slices: remaining (colored) starts at top and goes clockwise
        let slices = vec![
            DonutSlice {
                value: self.remaining,
                color: remaining_color,
            },
            DonutSlice {
                value: 1.0 - self.remaining,
                color: Color::DarkGray,
            },
        ];

        // Size the donut to fit the area
        let radius = (area.width.min(area.height * 2) as f64 / 4.0).max(3.0);
        let thickness = (radius * 0.4).max(1.0);

        DonutChart::new(slices)
            .radius(radius)
            .thickness(thickness)
            .render(area, buf);
    }
}
