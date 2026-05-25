use kurbo::Shape;
use serde::{Deserialize, Serialize};

use crate::{Bounds, Point, Segment};

/// How control-point handles behave at an anchor vertex.
///
/// This determines the constraint applied when dragging a control point:
/// the opposite handle is adjusted (or not) to maintain the invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VertexMode {
    /// Control points are fully independent.
    #[default]
    Corner,
    /// Control points are collinear through the anchor (same direction,
    /// but may differ in distance).
    Smooth,
    /// Control points are collinear AND equidistant (mirror reflections).
    Symmetric,
}

/// A subpath: a sequence of segments starting from a given point,
/// optionally closed back to the start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubPath {
    pub start: Point,
    pub segments: Vec<Segment>,
    pub closed: bool,
    /// Mode for each anchor point. Index 0 = start point,
    /// index i+1 = endpoint of segment i.
    /// Length should be `1 + segments.len()`.
    pub vertex_modes: Vec<VertexMode>,
}

impl SubPath {
    pub fn new(start: Point) -> Self {
        Self {
            start,
            segments: Vec::new(),
            closed: false,
            vertex_modes: vec![VertexMode::Corner],
        }
    }

    /// Push a segment and its endpoint's vertex mode.
    pub fn push_segment(&mut self, seg: Segment, mode: VertexMode) {
        self.segments.push(seg);
        self.vertex_modes.push(mode);
    }

    /// Total arc length of the subpath.
    pub fn arc_length(&self) -> f64 {
        let mut length = 0.0;
        let mut current = self.start;
        for seg in &self.segments {
            length += seg.arc_length(current);
            current = seg.endpoint();
        }
        length
    }

    /// Bounding box of the entire subpath (conservative).
    pub fn bounding_box(&self) -> Bounds {
        let mut bounds = Bounds::EMPTY.include_point(self.start);
        let mut current = self.start;
        for seg in &self.segments {
            bounds = bounds.union(seg.bounding_box(current));
            current = seg.endpoint();
        }
        bounds
    }
}

/// A complete path, composed of one or more subpaths.
/// This is the main geometry primitive in the scene graph.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Path {
    pub subpaths: Vec<SubPath>,
}

impl Path {
    pub fn new() -> Self {
        Self {
            subpaths: Vec::new(),
        }
    }

    pub fn bounding_box(&self) -> Bounds {
        self.subpaths
            .iter()
            .fold(Bounds::EMPTY, |acc, sp| acc.union(sp.bounding_box()))
    }

    pub fn is_empty(&self) -> bool {
        self.subpaths.is_empty()
    }

    /// Convert this path to a kurbo `BezPath`. Arc segments are flattened
    /// into cubic-bezier approximations using kurbo's SVG-compatible arc
    /// math. Subpaths' `closed` flag is honoured — closed subpaths emit a
    /// `ClosePath` element so kurbo's winding number matches the SVG fill
    /// semantics.
    ///
    /// `arc_tolerance` controls how tightly arcs are approximated by
    /// cubics — `0.1` is roughly invisible at typical zooms.
    pub fn to_kurbo(&self, arc_tolerance: f64) -> kurbo::BezPath {
        let mut out = kurbo::BezPath::new();
        for sp in &self.subpaths {
            out.move_to(kurbo::Point::from(sp.start));
            let mut current = sp.start;
            for seg in &sp.segments {
                match seg {
                    Segment::Line { to } => {
                        out.line_to(kurbo::Point::from(*to));
                    }
                    Segment::Quad { ctrl, to } => {
                        out.quad_to(kurbo::Point::from(*ctrl), kurbo::Point::from(*to));
                    }
                    Segment::Cubic { ctrl1, ctrl2, to } => {
                        out.curve_to(
                            kurbo::Point::from(*ctrl1),
                            kurbo::Point::from(*ctrl2),
                            kurbo::Point::from(*to),
                        );
                    }
                    Segment::Arc {
                        radii,
                        x_rotation,
                        large_arc,
                        sweep,
                        to,
                    } => {
                        let svg_arc = kurbo::SvgArc {
                            from: kurbo::Point::from(current),
                            to: kurbo::Point::from(*to),
                            radii: kurbo::Vec2::new(radii.x, radii.y),
                            x_rotation: *x_rotation,
                            large_arc: *large_arc,
                            sweep: *sweep,
                        };
                        if let Some(arc) = kurbo::Arc::from_svg_arc(&svg_arc) {
                            arc.to_cubic_beziers(arc_tolerance, |c1, c2, end| {
                                out.curve_to(c1, c2, end);
                            });
                        } else {
                            // Degenerate arc (zero radius / coincident endpoints) → line.
                            out.line_to(kurbo::Point::from(*to));
                        }
                    }
                }
                current = seg.endpoint();
            }
            if sp.closed {
                out.close_path();
            }
        }
        out
    }

    /// True iff `point` is inside the area filled by this path under the
    /// given fill rule. `even_odd` selects EvenOdd when true and NonZero
    /// when false (matches SVG `fill-rule` semantics).
    ///
    /// All subpaths contribute to winding regardless of their `closed`
    /// flag — SVG's fill rule treats every subpath as implicitly closed
    /// (a final straight-line close to the start) for the purpose of
    /// computing the filled area.
    pub fn contains_point(&self, point: Point, even_odd: bool) -> bool {
        // 0.1 is the conventional "invisible to the eye" arc tolerance.
        let bez = self.to_kurbo(0.1);
        let w = bez.winding(kurbo::Point::from(point));
        if even_odd { w & 1 != 0 } else { w != 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::point::Vec2;

    fn rect_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        let mut sp = SubPath::new(Point::new(x0, y0));
        sp.push_segment(
            Segment::Line {
                to: Point::new(x1, y0),
            },
            super::VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Line {
                to: Point::new(x1, y1),
            },
            super::VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Line {
                to: Point::new(x0, y1),
            },
            super::VertexMode::Corner,
        );
        sp.closed = true;
        Path { subpaths: vec![sp] }
    }

    #[test]
    fn contains_point_inside_rect() {
        let p = rect_path(0.0, 0.0, 10.0, 10.0);
        assert!(p.contains_point(Point::new(5.0, 5.0), false));
        assert!(p.contains_point(Point::new(5.0, 5.0), true));
    }

    #[test]
    fn contains_point_outside_rect() {
        let p = rect_path(0.0, 0.0, 10.0, 10.0);
        assert!(!p.contains_point(Point::new(15.0, 5.0), false));
        assert!(!p.contains_point(Point::new(-1.0, 5.0), false));
        assert!(!p.contains_point(Point::new(5.0, 20.0), false));
    }

    #[test]
    fn contains_point_donut_even_odd() {
        // Outer 0..10, inner 3..7. EvenOdd treats the hole as outside the
        // fill; NonZero — same here, since both subpaths wind the same way.
        let outer = rect_path(0.0, 0.0, 10.0, 10.0)
            .subpaths
            .into_iter()
            .next()
            .unwrap();
        let inner = rect_path(3.0, 3.0, 7.0, 7.0)
            .subpaths
            .into_iter()
            .next()
            .unwrap();
        let donut = Path {
            subpaths: vec![outer, inner],
        };
        // Point inside inner hole: EvenOdd → outside (hole).
        assert!(!donut.contains_point(Point::new(5.0, 5.0), true));
        // Point in the ring between outer and inner: inside.
        assert!(donut.contains_point(Point::new(1.0, 5.0), true));
        // Point outside everything.
        assert!(!donut.contains_point(Point::new(20.0, 5.0), true));
    }

    #[test]
    fn contains_point_arc_segment() {
        // Half-disk: line from (-5,0) to (5,0), then arc back. The arc
        // sweeps through the upper half (sweep=true with y-down means
        // the curve bulges to negative y in screen coords; for our test
        // we just verify a point clearly inside the half-disk hits and a
        // clearly-outside point misses).
        let mut sp = SubPath::new(Point::new(-5.0, 0.0));
        sp.push_segment(
            Segment::Line {
                to: Point::new(5.0, 0.0),
            },
            super::VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Arc {
                radii: Vec2::new(5.0, 5.0),
                x_rotation: 0.0,
                large_arc: false,
                sweep: true,
                to: Point::new(-5.0, 0.0),
            },
            super::VertexMode::Corner,
        );
        sp.closed = true;
        let p = Path { subpaths: vec![sp] };
        // Far outside.
        assert!(!p.contains_point(Point::new(100.0, 100.0), false));
        // Inside the disk on one side; whichever sign matches the sweep,
        // the OPPOSITE side is empty. Test both — exactly one should be true.
        let above = p.contains_point(Point::new(0.0, -2.0), false);
        let below = p.contains_point(Point::new(0.0, 2.0), false);
        assert!(
            above ^ below,
            "exactly one side of the diameter should be filled"
        );
    }
}
