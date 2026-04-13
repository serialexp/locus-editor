use serde::{Deserialize, Serialize};

use crate::{Bounds, Point};
use crate::point::Vec2;

/// A single segment in a path. All segments are defined relative to
/// an implicit "current point" (the endpoint of the previous segment,
/// or the subpath's start point for the first segment).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Segment {
    /// Straight line to `to`.
    Line {
        to: Point,
    },

    /// Quadratic Bezier curve with one control point.
    Quad {
        ctrl: Point,
        to: Point,
    },

    /// Cubic Bezier curve with two control points.
    Cubic {
        ctrl1: Point,
        ctrl2: Point,
        to: Point,
    },

    /// Elliptical arc (matches SVG arc semantics).
    /// `radii` are the x/y radii of the ellipse.
    /// `x_rotation` is the rotation of the ellipse's x-axis in radians.
    /// `large_arc` selects the larger of the two possible arcs.
    /// `sweep` selects clockwise (true) vs counter-clockwise (false).
    Arc {
        radii: Vec2,
        x_rotation: f64,
        large_arc: bool,
        sweep: bool,
        to: Point,
    },
}

impl Segment {
    /// The endpoint of this segment.
    pub fn endpoint(&self) -> Point {
        match self {
            Segment::Line { to }
            | Segment::Quad { to, .. }
            | Segment::Cubic { to, .. }
            | Segment::Arc { to, .. } => *to,
        }
    }

    /// Conservative bounding box (may overestimate for curves).
    /// `from` is the current point (start of this segment).
    pub fn bounding_box(&self, from: Point) -> Bounds {
        match self {
            Segment::Line { to } => Bounds::from_points([from, *to]),
            Segment::Quad { ctrl, to } => Bounds::from_points([from, *ctrl, *to]),
            Segment::Cubic { ctrl1, ctrl2, to } => {
                Bounds::from_points([from, *ctrl1, *ctrl2, *to])
            }
            Segment::Arc { to, .. } => {
                // TODO: compute tight arc bounds from the ellipse parameters.
                // For now, control-point hull is a rough overestimate.
                Bounds::from_points([from, *to])
            }
        }
    }
}
