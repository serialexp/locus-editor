use serde::{Deserialize, Serialize};

use crate::point::Vec2;
use crate::{Bounds, Point};

/// A single segment in a path. All segments are defined relative to
/// an implicit "current point" (the endpoint of the previous segment,
/// or the subpath's start point for the first segment).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Segment {
    /// Straight line to `to`.
    Line { to: Point },

    /// Quadratic Bezier curve with one control point.
    Quad { ctrl: Point, to: Point },

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
            Segment::Cubic { ctrl1, ctrl2, to } => Bounds::from_points([from, *ctrl1, *ctrl2, *to]),
            Segment::Arc { to, .. } => {
                // TODO: compute tight arc bounds from the ellipse parameters.
                // For now, control-point hull is a rough overestimate.
                Bounds::from_points([from, *to])
            }
        }
    }

    // ── Evaluation ──────────────────────────────────────────────────

    /// Evaluate the point on this segment at parameter `t` ∈ [0, 1].
    /// `from` is the implicit start point.
    pub fn eval_at(&self, from: Point, t: f64) -> Point {
        match self {
            Segment::Line { to } => lerp_pt(from, *to, t),
            Segment::Quad { ctrl, to } => eval_quad(from, *ctrl, *to, t),
            Segment::Cubic { ctrl1, ctrl2, to } => eval_cubic(from, *ctrl1, *ctrl2, *to, t),
            Segment::Arc {
                radii,
                x_rotation,
                large_arc,
                sweep,
                to,
            } => {
                if let Some(cp) =
                    ArcCenter::from_endpoint(from, *to, *radii, *x_rotation, *large_arc, *sweep)
                {
                    let angle = cp.start_angle + t * cp.sweep_angle;
                    cp.point_at_angle(angle)
                } else {
                    lerp_pt(from, *to, t)
                }
            }
        }
    }

    // ── Closest point ───────────────────────────────────────────────

    /// Find the closest point on this segment to `target`.
    /// `from` is the implicit start point.
    /// Returns `(t, closest_point, distance)`.
    pub fn closest_point(&self, from: Point, target: Point) -> (f64, Point, f64) {
        match self {
            Segment::Line { to } => closest_point_on_line(from, *to, target),
            _ => {
                // For curves: sample at N points and refine.
                closest_point_by_sampling(self, from, target, 64)
            }
        }
    }

    // ── Splitting ───────────────────────────────────────────────────

    /// Split this segment at parameter `t` ∈ [0, 1].
    /// `from` is the implicit start point.
    /// Returns `(first_half, second_half)`.
    pub fn split_at(&self, from: Point, t: f64) -> (Segment, Segment) {
        match self {
            Segment::Line { to } => {
                let mid = lerp_pt(from, *to, t);
                (Segment::Line { to: mid }, Segment::Line { to: *to })
            }
            Segment::Quad { ctrl, to } => split_quad(from, *ctrl, *to, t),
            Segment::Cubic { ctrl1, ctrl2, to } => split_cubic(from, *ctrl1, *ctrl2, *to, t),
            Segment::Arc {
                radii,
                x_rotation,
                large_arc,
                sweep,
                to,
            } => {
                if let Some(cp) =
                    ArcCenter::from_endpoint(from, *to, *radii, *x_rotation, *large_arc, *sweep)
                {
                    let mid_angle = cp.start_angle + t * cp.sweep_angle;
                    let mid_point = cp.point_at_angle(mid_angle);

                    let sweep1 = t * cp.sweep_angle;
                    let sweep2 = (1.0 - t) * cp.sweep_angle;

                    (
                        Segment::Arc {
                            radii: *radii,
                            x_rotation: *x_rotation,
                            large_arc: sweep1.abs() > std::f64::consts::PI,
                            sweep: *sweep,
                            to: mid_point,
                        },
                        Segment::Arc {
                            radii: *radii,
                            x_rotation: *x_rotation,
                            large_arc: sweep2.abs() > std::f64::consts::PI,
                            sweep: *sweep,
                            to: *to,
                        },
                    )
                } else {
                    // Degenerate arc — fall back to line split.
                    let mid = lerp_pt(from, *to, t);
                    (Segment::Line { to: mid }, Segment::Line { to: *to })
                }
            }
        }
    }
}

// ── Arc center parameterization (SVG spec F.6) ─────────────────────

/// Center parameterization of an elliptical arc, computed from the SVG
/// endpoint parameterization. This is the form needed for evaluation
/// and splitting.
struct ArcCenter {
    /// Center of the ellipse.
    cx: f64,
    cy: f64,
    /// (Possibly corrected) radii.
    rx: f64,
    ry: f64,
    /// X-axis rotation in radians.
    x_rotation: f64,
    /// Start angle on the unit circle (after un-rotating and un-scaling).
    start_angle: f64,
    /// Sweep angle (positive = clockwise, negative = counter-clockwise).
    sweep_angle: f64,
}

impl ArcCenter {
    /// Convert SVG endpoint arc parameters to center parameterization.
    /// Follows the SVG spec appendix F.6.5 / F.6.6.
    /// Returns `None` for degenerate arcs (zero radii, coincident endpoints).
    fn from_endpoint(
        from: Point,
        to: Point,
        radii: Vec2,
        x_rotation: f64,
        large_arc: bool,
        sweep: bool,
    ) -> Option<Self> {
        let mut rx = radii.x.abs();
        let mut ry = radii.y.abs();

        if rx < 1e-10 || ry < 1e-10 {
            return None;
        }

        let dx = (from.x - to.x) / 2.0;
        let dy = (from.y - to.y) / 2.0;

        if dx.abs() < 1e-10 && dy.abs() < 1e-10 {
            return None; // Coincident endpoints.
        }

        let cos_rot = x_rotation.cos();
        let sin_rot = x_rotation.sin();

        // Step 1: Compute (x1', y1') — transformed midpoint.
        let x1p = cos_rot * dx + sin_rot * dy;
        let y1p = -sin_rot * dx + cos_rot * dy;

        // Step 2: Correct radii if too small.
        let x1p2 = x1p * x1p;
        let y1p2 = y1p * y1p;
        let mut rx2 = rx * rx;
        let mut ry2 = ry * ry;

        let lambda = x1p2 / rx2 + y1p2 / ry2;
        if lambda > 1.0 {
            let s = lambda.sqrt();
            rx *= s;
            ry *= s;
            rx2 = rx * rx;
            ry2 = ry * ry;
        }

        // Step 3: Compute center' (cx', cy').
        let num = (rx2 * ry2 - rx2 * y1p2 - ry2 * x1p2).max(0.0);
        let den = rx2 * y1p2 + ry2 * x1p2;
        let sq = if den < 1e-10 { 0.0 } else { (num / den).sqrt() };
        let sign = if large_arc == sweep { -1.0 } else { 1.0 };

        let cxp = sign * sq * (rx * y1p / ry);
        let cyp = sign * sq * -(ry * x1p / rx);

        // Step 4: Compute center (cx, cy).
        let mx = (from.x + to.x) / 2.0;
        let my = (from.y + to.y) / 2.0;
        let cx = cos_rot * cxp - sin_rot * cyp + mx;
        let cy = sin_rot * cxp + cos_rot * cyp + my;

        // Step 5: Compute start angle and sweep angle.
        let ux = (x1p - cxp) / rx;
        let uy = (y1p - cyp) / ry;
        let vx = (-x1p - cxp) / rx;
        let vy = (-y1p - cyp) / ry;

        let start_angle = vec_angle(1.0, 0.0, ux, uy);
        let mut sweep_angle = vec_angle(ux, uy, vx, vy);

        if !sweep && sweep_angle > 0.0 {
            sweep_angle -= 2.0 * std::f64::consts::PI;
        } else if sweep && sweep_angle < 0.0 {
            sweep_angle += 2.0 * std::f64::consts::PI;
        }

        Some(ArcCenter {
            cx,
            cy,
            rx,
            ry,
            x_rotation,
            start_angle,
            sweep_angle,
        })
    }

    /// Evaluate the point on the ellipse at a given angle θ.
    fn point_at_angle(&self, angle: f64) -> Point {
        let cos_rot = self.x_rotation.cos();
        let sin_rot = self.x_rotation.sin();
        let cos_a = angle.cos();
        let sin_a = angle.sin();

        Point::new(
            self.cx + self.rx * cos_a * cos_rot - self.ry * sin_a * sin_rot,
            self.cy + self.rx * cos_a * sin_rot + self.ry * sin_a * cos_rot,
        )
    }
}

/// Angle between two vectors (u, v), in radians. Result is in [-π, π].
fn vec_angle(ux: f64, uy: f64, vx: f64, vy: f64) -> f64 {
    let dot = ux * vx + uy * vy;
    let cross = ux * vy - uy * vx;
    cross.atan2(dot)
}

// ── Geometry helpers ────────────────────────────────────────────────

fn lerp_pt(a: Point, b: Point, t: f64) -> Point {
    Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}

fn eval_quad(p0: Point, p1: Point, p2: Point, t: f64) -> Point {
    let a = lerp_pt(p0, p1, t);
    let b = lerp_pt(p1, p2, t);
    lerp_pt(a, b, t)
}

fn eval_cubic(p0: Point, p1: Point, p2: Point, p3: Point, t: f64) -> Point {
    let a = lerp_pt(p0, p1, t);
    let b = lerp_pt(p1, p2, t);
    let c = lerp_pt(p2, p3, t);
    let d = lerp_pt(a, b, t);
    let e = lerp_pt(b, c, t);
    lerp_pt(d, e, t)
}

/// Closest point on a line segment (exact solution).
fn closest_point_on_line(a: Point, b: Point, target: Point) -> (f64, Point, f64) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len_sq = dx * dx + dy * dy;

    let t = if len_sq < 1e-12 {
        0.0
    } else {
        ((target.x - a.x) * dx + (target.y - a.y) * dy) / len_sq
    }
    .clamp(0.0, 1.0);

    let pt = lerp_pt(a, b, t);
    let dist = target.distance(pt);
    (t, pt, dist)
}

/// Closest point on any segment by dense sampling + refinement.
fn closest_point_by_sampling(
    seg: &Segment,
    from: Point,
    target: Point,
    num_samples: usize,
) -> (f64, Point, f64) {
    let mut best_t = 0.0;
    let mut best_dist = f64::INFINITY;
    let mut best_pt = from;

    // Coarse pass.
    for i in 0..=num_samples {
        let t = i as f64 / num_samples as f64;
        let pt = seg.eval_at(from, t);
        let d = target.distance(pt);
        if d < best_dist {
            best_dist = d;
            best_t = t;
            best_pt = pt;
        }
    }

    // Refine: binary search around best_t.
    let mut lo = (best_t - 1.0 / num_samples as f64).max(0.0);
    let mut hi = (best_t + 1.0 / num_samples as f64).min(1.0);
    for _ in 0..16 {
        let t1 = lo + (hi - lo) / 3.0;
        let t2 = hi - (hi - lo) / 3.0;
        let d1 = target.distance(seg.eval_at(from, t1));
        let d2 = target.distance(seg.eval_at(from, t2));
        if d1 < d2 {
            hi = t2;
        } else {
            lo = t1;
        }
    }
    let t_final = (lo + hi) * 0.5;
    let pt_final = seg.eval_at(from, t_final);
    let d_final = target.distance(pt_final);

    if d_final < best_dist {
        (t_final, pt_final, d_final)
    } else {
        (best_t, best_pt, best_dist)
    }
}

/// De Casteljau split of a quadratic bezier at parameter t.
fn split_quad(p0: Point, p1: Point, p2: Point, t: f64) -> (Segment, Segment) {
    let a = lerp_pt(p0, p1, t);
    let b = lerp_pt(p1, p2, t);
    let mid = lerp_pt(a, b, t);

    (
        Segment::Quad { ctrl: a, to: mid },
        Segment::Quad { ctrl: b, to: p2 },
    )
}

/// De Casteljau split of a cubic bezier at parameter t.
fn split_cubic(p0: Point, p1: Point, p2: Point, p3: Point, t: f64) -> (Segment, Segment) {
    let a = lerp_pt(p0, p1, t);
    let b = lerp_pt(p1, p2, t);
    let c = lerp_pt(p2, p3, t);
    let d = lerp_pt(a, b, t);
    let e = lerp_pt(b, c, t);
    let mid = lerp_pt(d, e, t);

    (
        Segment::Cubic {
            ctrl1: a,
            ctrl2: d,
            to: mid,
        },
        Segment::Cubic {
            ctrl1: e,
            ctrl2: c,
            to: p3,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_eval_endpoints() {
        let seg = Segment::Line {
            to: Point::new(10.0, 0.0),
        };
        let from = Point::new(0.0, 0.0);
        assert_eq!(seg.eval_at(from, 0.0), from);
        assert_eq!(seg.eval_at(from, 1.0), Point::new(10.0, 0.0));
    }

    #[test]
    fn line_eval_midpoint() {
        let seg = Segment::Line {
            to: Point::new(10.0, 0.0),
        };
        let mid = seg.eval_at(Point::ZERO, 0.5);
        assert!((mid.x - 5.0).abs() < 1e-10);
        assert!(mid.y.abs() < 1e-10);
    }

    #[test]
    fn line_closest_point() {
        let seg = Segment::Line {
            to: Point::new(10.0, 0.0),
        };
        let (t, pt, dist) = seg.closest_point(Point::ZERO, Point::new(5.0, 3.0));
        assert!((t - 0.5).abs() < 1e-10);
        assert!((pt.x - 5.0).abs() < 1e-10);
        assert!((dist - 3.0).abs() < 1e-10);
    }

    #[test]
    fn line_split() {
        let seg = Segment::Line {
            to: Point::new(10.0, 0.0),
        };
        let (a, b) = seg.split_at(Point::ZERO, 0.5);
        assert_eq!(a.endpoint().x, 5.0);
        assert_eq!(b.endpoint().x, 10.0);
    }

    #[test]
    fn cubic_split_preserves_endpoints() {
        let seg = Segment::Cubic {
            ctrl1: Point::new(0.0, 10.0),
            ctrl2: Point::new(10.0, 10.0),
            to: Point::new(10.0, 0.0),
        };
        let from = Point::ZERO;
        let (a, b) = seg.split_at(from, 0.5);
        // The first half starts at `from` and ends at the midpoint.
        let mid = seg.eval_at(from, 0.5);
        assert!((a.endpoint().x - mid.x).abs() < 1e-10);
        assert!((a.endpoint().y - mid.y).abs() < 1e-10);
        // The second half ends at the original endpoint.
        assert!((b.endpoint().x - 10.0).abs() < 1e-10);
        assert!((b.endpoint().y).abs() < 1e-10);
    }

    #[test]
    fn cubic_closest_point_at_endpoint() {
        let seg = Segment::Cubic {
            ctrl1: Point::new(0.0, 10.0),
            ctrl2: Point::new(10.0, 10.0),
            to: Point::new(10.0, 0.0),
        };
        let (t, _pt, dist) = seg.closest_point(Point::ZERO, Point::new(10.0, 0.0));
        assert!(t > 0.9);
        assert!(dist < 0.1);
    }

    #[test]
    fn arc_eval_endpoints() {
        // Quarter-circle arc: right (1,0) → bottom (0,1), radius 1, sweep CW.
        let seg = Segment::Arc {
            radii: Vec2::new(1.0, 1.0),
            x_rotation: 0.0,
            large_arc: false,
            sweep: true,
            to: Point::new(0.0, 1.0),
        };
        let from = Point::new(1.0, 0.0);

        let p0 = seg.eval_at(from, 0.0);
        assert!((p0.x - 1.0).abs() < 1e-6, "start x: {}", p0.x);
        assert!(p0.y.abs() < 1e-6, "start y: {}", p0.y);

        let p1 = seg.eval_at(from, 1.0);
        assert!(p1.x.abs() < 1e-6, "end x: {}", p1.x);
        assert!((p1.y - 1.0).abs() < 1e-6, "end y: {}", p1.y);
    }

    #[test]
    fn arc_eval_midpoint() {
        // Quarter-circle: midpoint should be at 45° → (cos45, sin45) ≈ (0.707, 0.707)
        let seg = Segment::Arc {
            radii: Vec2::new(1.0, 1.0),
            x_rotation: 0.0,
            large_arc: false,
            sweep: true,
            to: Point::new(0.0, 1.0),
        };
        let from = Point::new(1.0, 0.0);
        let mid = seg.eval_at(from, 0.5);
        let expected = 1.0 / 2.0_f64.sqrt();
        assert!(
            (mid.x - expected).abs() < 1e-4,
            "mid.x = {}, expected {}",
            mid.x,
            expected
        );
        assert!(
            (mid.y - expected).abs() < 1e-4,
            "mid.y = {}, expected {}",
            mid.y,
            expected
        );
    }

    #[test]
    fn arc_split_produces_arcs() {
        let seg = Segment::Arc {
            radii: Vec2::new(5.0, 5.0),
            x_rotation: 0.0,
            large_arc: false,
            sweep: true,
            to: Point::new(0.0, 5.0),
        };
        let from = Point::new(5.0, 0.0);
        let (a, b) = seg.split_at(from, 0.5);

        // Both halves should be arcs, not lines.
        assert!(matches!(a, Segment::Arc { .. }), "first half should be Arc");
        assert!(
            matches!(b, Segment::Arc { .. }),
            "second half should be Arc"
        );

        // The midpoint of the split should be on the circle.
        let mid = a.endpoint();
        let dist_from_center = (mid.x * mid.x + mid.y * mid.y).sqrt();
        assert!(
            (dist_from_center - 5.0).abs() < 1e-4,
            "midpoint should be on circle, dist = {}",
            dist_from_center
        );

        // Second half should end at the original endpoint.
        assert!((b.endpoint().x).abs() < 1e-6);
        assert!((b.endpoint().y - 5.0).abs() < 1e-6);
    }

    #[test]
    fn arc_split_large_arc_flags() {
        // A large arc (> 180°) split at t=0.5 should produce two non-large arcs.
        let seg = Segment::Arc {
            radii: Vec2::new(1.0, 1.0),
            x_rotation: 0.0,
            large_arc: true,
            sweep: true,
            // Going from (1,0) to (0,-1) the long way around (270° CW).
            to: Point::new(0.0, -1.0),
        };
        let from = Point::new(1.0, 0.0);
        let (a, b) = seg.split_at(from, 0.5);

        // Each half is 135°, so neither should be large_arc.
        if let Segment::Arc { large_arc, .. } = a {
            assert!(!large_arc, "first half 135° should not be large_arc");
        }
        if let Segment::Arc { large_arc, .. } = b {
            assert!(!large_arc, "second half 135° should not be large_arc");
        }
    }
}
