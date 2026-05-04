//! User-facing tracing parameters.
//!
//! [`TraceParams`] is the surface the editor's trace dialog binds its
//! sliders to; it's converted to a [`vtracer::Config`] just before the
//! tracer runs. Built directly on top of vtracer's parameter set, so any
//! knob the underlying library exposes can be surfaced here without
//! adding intermediate semantics.

use visioncortex::PathSimplifyMode;
use vtracer::{ColorMode, Hierarchical};

use crate::TracePreset;

/// Curve-fitting mode for traced paths.
///
/// Mirrors [`vtracer::PathSimplifyMode`] but exposed under a friendlier
/// name (so `None` doesn't mean "missing" in a UI — it's the "raw pixel
/// boundary" option).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CurveMode {
    /// No simplification — the boundary follows pixel edges literally.
    /// Useful for pixel-art tracing where the staircase is the point.
    PixelArt,
    /// Polygonal simplification — straight segments only.
    Polygon,
    /// Spline fitting — smooth Béziers between corner vertices.
    #[default]
    Spline,
}

impl CurveMode {
    fn to_vtracer(self) -> PathSimplifyMode {
        match self {
            CurveMode::PixelArt => PathSimplifyMode::None,
            CurveMode::Polygon => PathSimplifyMode::Polygon,
            CurveMode::Spline => PathSimplifyMode::Spline,
        }
    }
}

/// Whether the tracer emits stacked or cut-out colour layers.
///
/// Re-exported as a `Copy` enum to insulate the dialog UI from
/// vtracer's `Clone`-only enum (egui ComboBox + radio buttons want
/// `PartialEq`/`Copy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HierarchicalMode {
    /// Each colour layer paints over the previous ones — produces nicer
    /// edges where regions meet.
    #[default]
    Stacked,
    /// Each layer is the literal region of that colour, with the layers
    /// below cut out. Useful when you want every traced shape to be a
    /// disjoint region.
    Cutout,
}

impl HierarchicalMode {
    fn to_vtracer(self) -> Hierarchical {
        match self {
            HierarchicalMode::Stacked => Hierarchical::Stacked,
            HierarchicalMode::Cutout => Hierarchical::Cutout,
        }
    }
}

/// Whether the tracer emits one path per quantised colour, or a single
/// black-and-white outline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraceColorMode {
    /// Multi-colour output. `color_precision` and `layer_difference`
    /// govern quantisation.
    #[default]
    Color,
    /// Single-channel binary tracing. Useful for line art and scans.
    Binary,
}

impl TraceColorMode {
    fn to_vtracer(self) -> ColorMode {
        match self {
            TraceColorMode::Color => ColorMode::Color,
            TraceColorMode::Binary => ColorMode::Binary,
        }
    }
}

/// User-facing tracing parameters. Mirrors every knob on
/// [`vtracer::Config`] except `max_iterations`, which governs an
/// internal optimisation loop and isn't useful to expose.
///
/// Field ranges follow vtracer's CLI conventions and are clamped here on
/// `to_vtracer_config` so a caller passing wild values gets a sane Config
/// rather than a panic deep in vtracer.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceParams {
    pub color_mode: TraceColorMode,
    pub hierarchical: HierarchicalMode,
    pub curve_mode: CurveMode,
    /// Minimum cluster *side length* (in pixels). Clusters with fewer
    /// pixels than `filter_speckle * filter_speckle` are dropped.
    /// Higher values = less noise. Range: 0..=128.
    pub filter_speckle: u32,
    /// Quantisation depth in bits per channel. Lower values produce
    /// fewer, more posterised colours. Range: 1..=8.
    /// Only used in `Color` mode.
    pub color_precision: u32,
    /// Threshold (in 0..=255 channel units) for merging adjacent layers.
    /// Higher values = more layer merging = simpler output.
    /// Only used in `Color` mode.
    pub layer_difference: u32,
    /// Corner threshold in degrees. Angles below this are treated as
    /// smooth (continuous curve); above, as a corner. Range: 0..=180.
    /// Lower values = more corners, higher = smoother curves.
    pub corner_threshold: u32,
    /// Minimum segment length (in pixels) before splice/curve fitting.
    /// Range: 0.0..=10.0. Higher values produce simpler paths.
    pub length_threshold: f64,
    /// Splice angle threshold in degrees. Curves whose tangent angle
    /// changes by less than this are spliced together. Range: 0..=180.
    pub splice_threshold: u32,
    /// Decimal places retained in path output coordinates. Range: 0..=8.
    /// Lower = smaller SVG, higher = more precise.
    pub path_precision: u32,
}

impl Default for TraceParams {
    fn default() -> Self {
        // Match vtracer's `Poster` preset — what the existing one-shot
        // import was hardcoded to. Keeps behaviour identical for users
        // who never touch the dialog.
        Self::from_preset(TracePreset::Poster)
    }
}

impl TraceParams {
    /// Build params populated from one of the named vtracer presets.
    pub fn from_preset(preset: TracePreset) -> Self {
        match preset {
            TracePreset::Bw => Self {
                color_mode: TraceColorMode::Binary,
                hierarchical: HierarchicalMode::Stacked,
                curve_mode: CurveMode::Spline,
                filter_speckle: 4,
                color_precision: 6,
                layer_difference: 16,
                corner_threshold: 60,
                length_threshold: 4.0,
                splice_threshold: 45,
                path_precision: 2,
            },
            TracePreset::Poster => Self {
                color_mode: TraceColorMode::Color,
                hierarchical: HierarchicalMode::Stacked,
                curve_mode: CurveMode::Spline,
                filter_speckle: 4,
                color_precision: 8,
                layer_difference: 16,
                corner_threshold: 60,
                length_threshold: 4.0,
                splice_threshold: 45,
                path_precision: 2,
            },
            TracePreset::Photo => Self {
                color_mode: TraceColorMode::Color,
                hierarchical: HierarchicalMode::Stacked,
                curve_mode: CurveMode::Spline,
                filter_speckle: 10,
                color_precision: 8,
                layer_difference: 48,
                corner_threshold: 180,
                length_threshold: 4.0,
                splice_threshold: 45,
                path_precision: 2,
            },
        }
    }

    /// Convert to a [`vtracer::Config`], clamping each field to the
    /// range vtracer accepts. Out-of-range inputs are silently snapped
    /// rather than rejected — the dialog already constrains its sliders,
    /// and a programmatic caller passing junk gets a usable result
    /// instead of a panic.
    pub fn to_vtracer_config(&self) -> vtracer::Config {
        vtracer::Config {
            color_mode: self.color_mode.to_vtracer(),
            hierarchical: self.hierarchical.to_vtracer(),
            mode: self.curve_mode.to_vtracer(),
            filter_speckle: (self.filter_speckle as usize).min(128),
            color_precision: (self.color_precision as i32).clamp(1, 8),
            layer_difference: (self.layer_difference as i32).clamp(0, 128),
            corner_threshold: (self.corner_threshold as i32).clamp(0, 180),
            length_threshold: self.length_threshold.clamp(0.0, 10.0),
            // Internal optimiser cap; vtracer's CLI default is 10 and
            // there's no good reason to expose it as a knob — extra
            // iterations strictly improve the fit at trace-time cost
            // that's already dwarfed by the rest of the pipeline.
            max_iterations: 10,
            splice_threshold: (self.splice_threshold as i32).clamp(0, 180),
            path_precision: Some(self.path_precision.min(8)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_poster_preset() {
        assert_eq!(
            TraceParams::default(),
            TraceParams::from_preset(TracePreset::Poster)
        );
    }

    #[test]
    fn to_vtracer_config_clamps_extremes() {
        let p = TraceParams {
            filter_speckle: 9_999,
            color_precision: 99,
            corner_threshold: 9_999,
            length_threshold: 100.0,
            splice_threshold: 9_999,
            path_precision: 99,
            ..TraceParams::default()
        };
        let cfg = p.to_vtracer_config();
        assert!(cfg.filter_speckle <= 128);
        assert!(cfg.color_precision <= 8);
        assert!(cfg.corner_threshold <= 180);
        assert!(cfg.length_threshold <= 10.0);
        assert!(cfg.splice_threshold <= 180);
        assert_eq!(cfg.path_precision, Some(8));
    }

    #[test]
    fn bw_preset_uses_binary_mode() {
        let p = TraceParams::from_preset(TracePreset::Bw);
        assert_eq!(p.color_mode, TraceColorMode::Binary);
        let cfg = p.to_vtracer_config();
        assert!(matches!(cfg.color_mode, ColorMode::Binary));
    }
}
