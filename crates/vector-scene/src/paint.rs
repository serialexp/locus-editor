use serde::{Deserialize, Serialize};
use vector_geom::{Color, Point};

use crate::scene::NodeId;

/// Reference to a paint source. A paint is either a solid color (inline)
/// or a reference to a gradient/pattern node elsewhere in the scene tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PaintRef {
    /// Inline solid color — no reference needed.
    Solid(Color),
    /// Reference to a paint node (gradient or pattern) by its stable ID.
    Ref(NodeId),
}

/// A gradient definition, stored as node data in the scene tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gradient {
    pub kind: GradientKind,
    pub stops: Vec<ColorStop>,
    pub interpolation: InterpolationSpace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GradientKind {
    Linear {
        start: Point,
        end: Point,
    },
    Radial {
        center: Point,
        radius: f64,
        focal: Point,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ColorStop {
    pub offset: f32, // 0.0..=1.0
    pub color: Color,
}

/// Which color space to interpolate gradient colors in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InterpolationSpace {
    /// Standard linear RGB interpolation.
    #[default]
    LinearRgb,
    /// Perceptual interpolation via OKLab — produces nicer gradients
    /// between dissimilar hues (e.g. red→blue through purple, not mud).
    OkLab,
}

/// Paint data that lives as a node variant in the scene tree.
/// Patterns are just Group nodes referenced as a paint, so they don't
/// need a dedicated paint type — they use PaintRef::Ref pointing at
/// a regular group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Paint {
    Gradient(Gradient),
}
