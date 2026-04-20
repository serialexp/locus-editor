use serde::{Deserialize, Serialize};
use vector_geom::{Affine, Bounds, Path};

use crate::paint::Paint;
use crate::scene::NodeId;
use crate::style::Style;

/// A node in the scene graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Human-readable label (shown in layer panel, corresponds to SVG id).
    pub label: String,

    /// Local transform relative to parent.
    pub transform: Affine,

    /// What this node contains.
    pub data: NodeData,

    /// Ordered children (front-to-back: last child draws on top).
    pub children: Vec<NodeId>,

    /// Whether this node is visible.
    pub visible: bool,

    /// Whether this node is locked (not selectable/editable).
    pub locked: bool,
}

/// The payload that distinguishes node types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeData {
    /// A group — just a container with a transform.
    /// Also used for SVG `<defs>` (a non-rendered group)
    /// and patterns (a group referenced as a paint tile).
    Group {
        /// If true, this group's contents are not rendered directly.
        /// They exist only to be referenced (gradients, patterns, symbols).
        is_defs: bool,
    },

    /// A vector path with fill/stroke styling.
    Path { path: Path, style: Style },

    /// A paint definition (gradient or pattern). Lives in the defs subtree
    /// and is referenced via `PaintRef::Ref`.
    Paint(Paint),

    /// Editable text. The rendered glyph outlines are computed on demand
    /// (by vector-text) and cached — not stored here.
    Text(TextData),
}

impl NodeData {
    /// Compute the world-space visual bounding box for this node's own
    /// geometry (not its children), including the visible stroke area.
    ///
    /// For Path nodes, this tessellates the path (fill + stroke) and takes
    /// the exact AABB of the resulting vertices in local space, then
    /// transforms to world space. This is tighter than a `stroke_width / 2`
    /// expansion — a diagonal stroke expands only by `width/2 · cos(θ)`
    /// perpendicular to its direction, not by the full half-width on every
    /// side.
    ///
    /// For Text nodes, we don't yet have the glyph path geometry available
    /// here, so we fall back to the font-metric bounds expanded by a
    /// conservative `stroke_width / 2`. For Group, Paint, and other
    /// non-drawing nodes, returns `Bounds::EMPTY`.
    pub fn visual_bounds(&self, world: Affine) -> Bounds {
        match self {
            NodeData::Path { path, style } => {
                let has_fill = style.fill.is_some();
                let stroke_params = style.stroke.as_ref().map(|s| s.style.bounds_params());
                let local = vector_tess::path_visual_bounds(path, has_fill, stroke_params.as_ref());
                local.transform(world)
            }
            NodeData::Text(text) => {
                let local =
                    vector_text::text_bounds(&text.content, &text.font_family, text.font_size);
                let expansion = text
                    .style
                    .stroke
                    .as_ref()
                    .map(|s| s.style.width / 2.0)
                    .unwrap_or(0.0);
                let expanded = local.expand(expansion);
                expanded.transform(world)
            }
            _ => Bounds::EMPTY,
        }
    }
}

/// Data for a text node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextData {
    pub content: String,
    pub font_family: String,
    pub font_size: f64,
    pub style: Style,
    // TODO: font weight, style (italic), letter-spacing, line-height, alignment
}

impl Node {
    /// Create a new empty group node.
    pub fn group(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            transform: Affine::IDENTITY,
            data: NodeData::Group { is_defs: false },
            children: Vec::new(),
            visible: true,
            locked: false,
        }
    }

    /// Create a new text node with default style (black fill, no stroke).
    pub fn text(label: impl Into<String>, text_data: TextData) -> Self {
        Self {
            label: label.into(),
            transform: Affine::IDENTITY,
            data: NodeData::Text(text_data),
            children: Vec::new(),
            visible: true,
            locked: false,
        }
    }

    /// Create a new path node with default style (black fill, no stroke).
    pub fn path(label: impl Into<String>, path: Path) -> Self {
        Self {
            label: label.into(),
            transform: Affine::IDENTITY,
            data: NodeData::Path {
                path,
                style: Style::default(),
            },
            children: Vec::new(),
            visible: true,
            locked: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaintRef;
    use crate::style::{LineCap, LineJoin, Stroke, StrokeStyle};
    use vector_geom::{Color, Point, Segment, SubPath, VertexMode};

    fn square_path() -> Path {
        // 10×10 square from (0,0) to (10,10).
        let mut sp = SubPath::new(Point::new(0.0, 0.0));
        sp.push_segment(
            Segment::Line {
                to: Point::new(10.0, 0.0),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Line {
                to: Point::new(10.0, 10.0),
            },
            VertexMode::Corner,
        );
        sp.push_segment(
            Segment::Line {
                to: Point::new(0.0, 10.0),
            },
            VertexMode::Corner,
        );
        sp.closed = true;
        let mut path = Path::new();
        path.subpaths.push(sp);
        path
    }

    fn stroke(width: f64) -> Stroke {
        Stroke {
            paint: PaintRef::Solid(Color::BLACK),
            style: StrokeStyle {
                width,
                cap: LineCap::Butt,
                join: LineJoin::Miter,
                miter_limit: 4.0,
                dash: None,
            },
            opacity: 1.0,
        }
    }

    #[test]
    fn visual_bounds_no_stroke_matches_geometry() {
        let node = NodeData::Path {
            path: square_path(),
            style: Style::default(),
        };
        let b = node.visual_bounds(Affine::IDENTITY);
        assert_eq!(b.min, Point::new(0.0, 0.0));
        assert_eq!(b.max, Point::new(10.0, 10.0));
    }

    #[test]
    fn visual_bounds_includes_stroke() {
        // A 10px stroke should expand bounds by 5px on all sides.
        let node = NodeData::Path {
            path: square_path(),
            style: Style {
                fill: None,
                stroke: Some(stroke(10.0)),
            },
        };
        let b = node.visual_bounds(Affine::IDENTITY);
        assert_eq!(b.min, Point::new(-5.0, -5.0));
        assert_eq!(b.max, Point::new(15.0, 15.0));
        assert_eq!(b.width(), 20.0);
        assert_eq!(b.height(), 20.0);
    }

    #[test]
    fn visual_bounds_stroke_scales_with_transform() {
        // Stroke expansion happens in local space, so it scales with the
        // world transform — a 2x scale makes a 10px stroke cover 20px in
        // world space, so bounds expand by 10px in world.
        let node = NodeData::Path {
            path: square_path(),
            style: Style {
                fill: None,
                stroke: Some(stroke(10.0)),
            },
        };
        let scale_2x = Affine {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            tx: 0.0,
            ty: 0.0,
        };
        let b = node.visual_bounds(scale_2x);
        assert_eq!(b.min, Point::new(-10.0, -10.0));
        assert_eq!(b.max, Point::new(30.0, 30.0));
    }

    #[test]
    fn visual_bounds_group_returns_empty() {
        let node = NodeData::Group { is_defs: false };
        let b = node.visual_bounds(Affine::IDENTITY);
        assert!(b.is_empty());
    }

    #[test]
    fn visual_bounds_diagonal_stroke_is_tighter_than_half_width() {
        // A 45° line from (0,0) to (10,10) with a width-10 butt-cap stroke.
        // A naive `width/2` expansion on every side gives a 20×20 bounds
        // (-5,-5)..(15,15). The actual stroked region, however, is a
        // rectangle rotated 45° — its AABB is only ~17.07×17.07.
        let mut sp = SubPath::new(Point::new(0.0, 0.0));
        sp.push_segment(
            Segment::Line {
                to: Point::new(10.0, 10.0),
            },
            VertexMode::Corner,
        );
        let mut path = Path::new();
        path.subpaths.push(sp);

        let node = NodeData::Path {
            path,
            style: Style {
                fill: None,
                stroke: Some(stroke(10.0)),
            },
        };
        let b = node.visual_bounds(Affine::IDENTITY);

        // Perpendicular half-width extension: 5 · sin(45°) = 5 · √2/2 ≈ 3.536.
        let expected_extend = 5.0 * (2.0_f64).sqrt() / 2.0;
        let tol = 0.05;

        assert!(
            (b.min.x - (-expected_extend)).abs() < tol,
            "min.x {} vs expected {}",
            b.min.x,
            -expected_extend
        );
        assert!(
            (b.min.y - (-expected_extend)).abs() < tol,
            "min.y {} vs expected {}",
            b.min.y,
            -expected_extend
        );
        assert!(
            (b.max.x - (10.0 + expected_extend)).abs() < tol,
            "max.x {} vs expected {}",
            b.max.x,
            10.0 + expected_extend
        );
        assert!(
            (b.max.y - (10.0 + expected_extend)).abs() < tol,
            "max.y {} vs expected {}",
            b.max.y,
            10.0 + expected_extend
        );

        // Sanity check: should be strictly tighter than the naive 20×20.
        assert!(b.width() < 19.0, "width {} should be < 19.0", b.width());
        assert!(b.height() < 19.0, "height {} should be < 19.0", b.height());
    }
}
