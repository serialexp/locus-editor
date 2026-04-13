use serde::{Deserialize, Serialize};
use vector_geom::Color;

use crate::paint::PaintRef;

/// Visual style applied to a path or shape node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Style {
    pub fill: Option<Fill>,
    pub stroke: Option<Stroke>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fill: Some(Fill {
                paint: PaintRef::Solid(Color::BLACK),
                rule: FillRule::NonZero,
                opacity: 1.0,
            }),
            stroke: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub paint: PaintRef,
    pub rule: FillRule,
    pub opacity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stroke {
    pub paint: PaintRef,
    pub style: StrokeStyle,
    pub opacity: f32,
}

/// Stroke parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrokeStyle {
    pub width: f64,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: f64,
    /// Dash array + offset. `None` means solid stroke.
    pub dash: Option<DashPattern>,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        Self {
            width: 1.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: 4.0,
            dash: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashPattern {
    pub array: Vec<f64>,
    pub offset: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FillRule {
    NonZero,
    EvenOdd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineCap {
    Butt,
    Round,
    Square,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineJoin {
    Miter,
    Round,
    Bevel,
}
