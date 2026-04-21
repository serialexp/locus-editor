//! Snapping configuration. Editor preference only — not stored in SVG.

/// Snapping configuration — editor preference, not stored in SVG.
#[derive(Clone)]
pub(crate) struct SnapSettings {
    /// Whether grid snapping is enabled.
    pub(crate) grid_enabled: bool,
    /// Grid spacing in canvas units.
    pub(crate) grid_size: f64,
}

impl Default for SnapSettings {
    fn default() -> Self {
        Self {
            grid_enabled: false,
            grid_size: 1.0,
        }
    }
}

impl SnapSettings {
    /// Snap a canvas-space coordinate according to current settings.
    pub(crate) fn snap(&self, pos: [f64; 2]) -> [f64; 2] {
        if self.grid_enabled {
            let g = self.grid_size;
            [(pos[0] / g).round() * g, (pos[1] / g).round() * g]
        } else {
            pos
        }
    }
}
