//! Canvas pan/zoom camera. Pure math, no GPU or winit dependencies.

/// Camera state for canvas pan/zoom.
pub(crate) struct Camera {
    /// Offset in screen pixels (how much the canvas origin has been dragged).
    pub(crate) pan: [f32; 2],
    /// Zoom level (1.0 = 100%).
    pub(crate) zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            pan: [0.0, 0.0],
            zoom: 1.0,
        }
    }
}

impl Camera {
    /// Convert screen pixel coordinates to canvas (scene) coordinates.
    pub(crate) fn screen_to_canvas(&self, screen_x: f32, screen_y: f32) -> [f32; 2] {
        [
            (screen_x - self.pan[0]) / self.zoom,
            (screen_y - self.pan[1]) / self.zoom,
        ]
    }

    /// Set pan and zoom so that `bounds` (in canvas coordinates) fits inside
    /// the screen-space rectangle described by `viewport` (min_x, min_y, width,
    /// height in screen pixels), with some padding.
    pub(crate) fn zoom_to_fit(&mut self, bounds: locus_geom::Bounds, viewport: [f32; 4]) {
        if bounds.is_empty() {
            return;
        }
        let [vx, vy, vw, vh] = viewport;
        let content_w = bounds.width() as f32;
        let content_h = bounds.height() as f32;
        if content_w <= 0.0 || content_h <= 0.0 || vw <= 0.0 || vh <= 0.0 {
            return;
        }

        // Leave 5% padding on each side
        let padding_frac = 0.05;
        let usable_w = vw * (1.0 - 2.0 * padding_frac);
        let usable_h = vh * (1.0 - 2.0 * padding_frac);

        // Zoom to fit the smaller axis
        self.zoom = (usable_w / content_w).min(usable_h / content_h);
        self.zoom = self.zoom.clamp(0.05, 100.0);

        // Pan so that the center of the content maps to the center of the viewport.
        // screen = canvas * zoom + pan  =>  pan = screen_center - canvas_center * zoom
        let center = bounds.center();
        let viewport_center_x = vx + vw * 0.5;
        let viewport_center_y = vy + vh * 0.5;
        self.pan[0] = viewport_center_x - center.x as f32 * self.zoom;
        self.pan[1] = viewport_center_y - center.y as f32 * self.zoom;
    }

    /// Zoom by a factor, keeping the given screen point fixed.
    pub(crate) fn zoom_at(&mut self, factor: f32, screen_x: f32, screen_y: f32) {
        // Point in canvas coords before zoom
        let before = self.screen_to_canvas(screen_x, screen_y);
        self.zoom *= factor;
        self.zoom = self.zoom.clamp(0.05, 100.0);
        // Adjust pan so that `before` maps back to (screen_x, screen_y)
        self.pan[0] = screen_x - before[0] * self.zoom;
        self.pan[1] = screen_y - before[1] * self.zoom;
    }
}
