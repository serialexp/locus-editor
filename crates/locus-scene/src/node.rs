use std::sync::Arc;

use locus_geom::{Affine, Bounds, Path, Point};
use serde::{Deserialize, Serialize};

use crate::paint::{Paint, PaintRef};
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

impl Node {
    /// True iff this node should respond to user input — visible and not
    /// locked. Hit-testing, marquee selection, and edit tools should gate
    /// on this rather than checking `visible` alone.
    ///
    /// Note: `locked` is a *behavioural* flag (not pickable, not editable),
    /// not a *rendering* flag. Locked nodes still draw normally; they're
    /// just inert under the cursor.
    #[inline]
    pub fn is_interactive(&self) -> bool {
        self.visible && !self.locked
    }

    /// Invoke `f` once for each `PaintRef` carried by this node's style —
    /// fill first, then stroke. Returns immediately for node kinds that
    /// don't carry a style (groups in Regular mode, paint defs, raster
    /// images).
    ///
    /// Used by code that needs to enumerate paint references (palette
    /// scanning, gradient ref counting, on-canvas handle rendering).
    /// Centralising the fill/stroke walk here keeps the four kinds of
    /// styled node — Path, Boolean group, Text — in one place.
    pub fn for_each_paint_ref(&self, mut f: impl FnMut(&PaintRef)) {
        let style = match &self.data {
            NodeData::Path { style, .. } => Some(style),
            NodeData::Group {
                kind: GroupKind::Boolean { style, .. },
                ..
            } => Some(style),
            NodeData::Text(t) => Some(&t.style),
            _ => None,
        };
        if let Some(style) = style {
            if let Some(f_) = &style.fill {
                f(&f_.paint);
            }
            if let Some(s) = &style.stroke {
                f(&s.paint);
            }
        }
    }
}

/// The kind of a group — either a plain container, or a non-destructive
/// boolean compound whose rendered geometry is the computed op over its
/// descendant paths.
///
/// For boolean groups the group itself owns a `Style` (applied to the
/// computed result); the individual child paths' styles are ignored while
/// the group is in Boolean mode, but preserved so switching back to
/// `Regular` restores them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum GroupKind {
    /// Ordinary group — children render independently in order.
    #[default]
    Regular,
    /// Non-destructive boolean compound. The group renders as a single
    /// shape computed from descendant path geometry under `op`, styled
    /// with `style`. Children remain individually editable.
    Boolean { op: BoolOp, style: Style },
}

/// The boolean operation applied to a `GroupKind::Boolean`'s descendants.
///
/// `Union`, `Intersect`, and `Exclude` are symmetric and n-ary (order
/// doesn't matter). `Difference` is asymmetric: the bottom-most descendant
/// in z-order is the base, and every other descendant is subtracted from
/// it — so things stacked on top visually cut holes in what's below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoolOp {
    Union,
    Difference,
    Intersect,
    Exclude,
}

/// The payload that distinguishes node types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeData {
    /// A group — a container with a transform, and optionally a boolean-op
    /// kind that makes the group render as a computed shape over its
    /// descendants rather than as a plain container.
    ///
    /// Also used for SVG `<defs>` (a non-rendered group) and patterns (a
    /// group referenced as a paint tile).
    Group {
        /// If true, this group's contents are not rendered directly.
        /// They exist only to be referenced (gradients, patterns, symbols).
        is_defs: bool,
        /// Whether this group behaves as a plain container or as a
        /// non-destructive boolean compound (union/difference/intersect/
        /// exclude over descendant path geometry).
        kind: GroupKind,
    },

    /// A vector path with fill/stroke styling.
    Path { path: Path, style: Style },

    /// A paint definition (gradient or pattern). Lives in the defs subtree
    /// and is referenced via `PaintRef::Ref`.
    Paint(Paint),

    /// Editable text. The rendered glyph outlines are computed on demand
    /// (by locus-text) and cached — not stored here.
    Text(TextData),

    /// A raster image displayed inside a local-space rectangle from
    /// `(0, 0)` to `(width, height)`. The Node's `transform` places the
    /// box in world space, so positioning, scaling and rotating an image
    /// works the same way as for any other node.
    ///
    /// `width` and `height` are decoupled from the source pixel dimensions:
    /// resampling to a different display size is just a transform / size
    /// change, no re-decode. By default we initialise them to the source
    /// pixel dimensions (one document unit per source pixel).
    ///
    /// Pixels are held as `Arc<RasterImage>` so undo snapshots and clones
    /// are cheap — the buffer is shared until somebody actually edits it.
    /// The renderer also keys its texture-upload cache on the `Arc`'s
    /// pointer, so swapping the image (replacing the `Arc`) invalidates
    /// the cache automatically.
    Raster {
        image: Arc<RasterImage>,
        width: f64,
        height: f64,
    },
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
                let local = locus_tess::path_visual_bounds(path, has_fill, stroke_params.as_ref());
                local.transform(world)
            }
            NodeData::Text(text) => {
                let local =
                    locus_text::text_bounds(&text.content, &text.font_family, text.font_size);
                let expansion = text
                    .style
                    .stroke
                    .as_ref()
                    .map(|s| s.style.width / 2.0)
                    .unwrap_or(0.0);
                let expanded = local.expand(expansion);
                expanded.transform(world)
            }
            NodeData::Raster { width, height, .. } => {
                // The image's local box is (0,0)..(w,h). Transform that to
                // world space; under non-axis-aligned transforms the AABB
                // of the four corners is the right answer.
                let local = Bounds::new(Point::new(0.0, 0.0), Point::new(*width, *height));
                local.transform(world)
            }
            _ => Bounds::EMPTY,
        }
    }
}

/// Decoded raster image data. Pixels are RGBA8, row-major, treated as sRGB.
///
/// We deliberately store decoded pixels rather than the encoded source
/// (PNG/JPG/etc.):
/// * The renderer wants RGBA8 anyway — keeping the encoded blob would mean
///   decoding on every GPU upload.
/// * SVG export can re-encode to PNG when needed; we don't preserve the
///   user's original byte stream.
///
/// Use `Arc<RasterImage>` from a `NodeData::Raster` so that node clones
/// (undo snapshots) share the underlying buffer instead of duplicating it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RasterImage {
    /// Raw RGBA8 pixels, row-major, length must equal
    /// `pixel_width * pixel_height * 4`.
    pub pixels: Vec<u8>,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl RasterImage {
    /// Build a new `RasterImage`. Panics in debug builds if the buffer
    /// length doesn't match `pixel_width * pixel_height * 4`.
    pub fn new(pixels: Vec<u8>, pixel_width: u32, pixel_height: u32) -> Self {
        debug_assert_eq!(
            pixels.len(),
            (pixel_width as usize) * (pixel_height as usize) * 4,
            "RasterImage pixel buffer length must match width*height*4"
        );
        Self {
            pixels,
            pixel_width,
            pixel_height,
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
            data: NodeData::Group {
                is_defs: false,
                kind: GroupKind::Regular,
            },
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

    /// Create a new raster image node. The local-space box runs from
    /// `(0, 0)` to `(width, height)`; use the node's `transform` to place
    /// it on the canvas.
    pub fn raster(
        label: impl Into<String>,
        image: Arc<RasterImage>,
        width: f64,
        height: f64,
    ) -> Self {
        Self {
            label: label.into(),
            transform: Affine::IDENTITY,
            data: NodeData::Raster {
                image,
                width,
                height,
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
    use locus_geom::{Color, Point, Segment, SubPath, VertexMode};

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
        let node = NodeData::Group {
            is_defs: false,
            kind: GroupKind::Regular,
        };
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

    fn dummy_raster(w: u32, h: u32) -> Arc<RasterImage> {
        // 1×1 transparent placeholder, scaled by claiming larger logical
        // dimensions. We use w*h*4 zero bytes to keep the constructor happy.
        Arc::new(RasterImage::new(
            vec![0u8; (w as usize) * (h as usize) * 4],
            w,
            h,
        ))
    }

    #[test]
    fn visual_bounds_raster_matches_box() {
        let node = NodeData::Raster {
            image: dummy_raster(2, 2),
            width: 200.0,
            height: 100.0,
        };
        let b = node.visual_bounds(Affine::IDENTITY);
        assert_eq!(b.min, Point::new(0.0, 0.0));
        assert_eq!(b.max, Point::new(200.0, 100.0));
    }

    #[test]
    fn visual_bounds_raster_scales_with_transform() {
        // 100×50 image scaled 3× and translated. The bounds should reflect
        // the world-space corners of the local box.
        let node = NodeData::Raster {
            image: dummy_raster(2, 2),
            width: 100.0,
            height: 50.0,
        };
        let xform = Affine {
            a: 3.0,
            b: 0.0,
            c: 0.0,
            d: 3.0,
            tx: 10.0,
            ty: 20.0,
        };
        let b = node.visual_bounds(xform);
        assert_eq!(b.min, Point::new(10.0, 20.0));
        assert_eq!(b.max, Point::new(310.0, 170.0));
    }

    #[test]
    fn is_interactive_requires_visible_and_unlocked() {
        let img = dummy_raster(2, 2);
        let mut node = Node::raster("r", img, 10.0, 10.0);
        assert!(node.is_interactive());

        node.locked = true;
        assert!(!node.is_interactive(), "locked nodes are not interactive");
        node.locked = false;
        node.visible = false;
        assert!(!node.is_interactive(), "hidden nodes are not interactive");

        node.locked = true;
        node.visible = false;
        assert!(
            !node.is_interactive(),
            "hidden+locked is also not interactive"
        );
    }

    #[test]
    #[should_panic]
    fn raster_image_new_panics_on_wrong_buffer_size() {
        // 4×4 = 16 pixels = 64 bytes; passing 60 bytes must trip the debug
        // assert. (Release builds skip the check by design.)
        let _ = RasterImage::new(vec![0u8; 60], 4, 4);
    }
}
