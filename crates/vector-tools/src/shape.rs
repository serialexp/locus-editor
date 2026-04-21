//! Rectangle and Ellipse drawing tools.
//!
//! Both follow the same interaction: press to set the anchor corner,
//! drag to set the opposite corner, release to finalize. During the drag
//! a live-preview node is inserted into the scene and updated each frame.

use vector_geom::{Color, Path, Point, Segment, SubPath, Vec2, VertexMode};
use vector_scene::{NodeData, NodeId, Scene};

use crate::ToolType;

/// Transient state for the rectangle / ellipse draw interaction.
#[derive(Default)]
pub struct ShapeDrawState {
    /// The anchor corner (where the mouse was pressed), in canvas coords.
    anchor: Option<[f64; 2]>,
    /// The node being drawn (live-preview). `None` before first press.
    preview_node: Option<NodeId>,
}

impl ShapeDrawState {
    /// Handle mouse press — record anchor and insert a zero-size preview node.
    pub fn on_press(&mut self, scene: &mut Scene, canvas_pos: [f64; 2], tool: ToolType) {
        // Remove stale preview if any
        self.cancel(scene);

        self.anchor = Some(canvas_pos);

        // Insert a degenerate (zero-size) shape so the structure panel shows it
        let path = build_shape_path(tool, canvas_pos, canvas_pos);
        let mut node = vector_scene::Node::path(tool.name(), path);

        // Default fill: steel blue, same as demo triangle
        if let NodeData::Path { ref mut style, .. } = node.data {
            style.fill = Some(vector_scene::style::Fill {
                paint: vector_scene::PaintRef::Solid(Color::from_srgb8(70, 130, 180, 255)),
                rule: vector_scene::FillRule::NonZero,
                opacity: 1.0,
            });
        }

        let root = scene.root();
        self.preview_node = scene.insert(root, node);
    }

    /// Handle mouse drag — update the preview shape to match the new extent.
    /// Returns `true` if the scene changed (caller should mark dirty + redraw).
    pub fn on_drag(&mut self, scene: &mut Scene, canvas_pos: [f64; 2], tool: ToolType) -> bool {
        let Some(anchor) = self.anchor else {
            return false;
        };
        let Some(node_id) = self.preview_node else {
            return false;
        };
        let new_path = build_shape_path(tool, anchor, canvas_pos);
        scene.set_path_data(node_id, new_path).is_some()
    }

    /// Handle mouse release — finalize the shape. Returns the node ID of the
    /// new shape (so the caller can e.g. select it).
    pub fn on_release(&mut self, scene: &mut Scene) -> Option<NodeId> {
        let node_id = self.preview_node.take();
        self.anchor = None;

        // If the shape is degenerate (zero-size), remove it
        if let Some(id) = node_id {
            let is_degenerate = scene
                .get(id)
                .and_then(|n| match &n.data {
                    NodeData::Path { path, .. } => {
                        let b = path.bounding_box();
                        Some(b.width() < 0.5 && b.height() < 0.5)
                    }
                    _ => None,
                })
                .unwrap_or(true);

            if is_degenerate {
                scene.remove(id);
                return None;
            }
        }

        node_id
    }

    /// Cancel an in-progress draw — remove the preview node if any.
    pub fn cancel(&mut self, scene: &mut Scene) {
        if let Some(id) = self.preview_node.take() {
            scene.remove(id);
        }
        self.anchor = None;
    }

    /// Whether a draw is currently in progress.
    pub fn is_drawing(&self) -> bool {
        self.anchor.is_some()
    }
}

/// Build the path geometry for a shape tool given two opposite corners.
fn build_shape_path(tool: ToolType, a: [f64; 2], b: [f64; 2]) -> Path {
    match tool {
        ToolType::Rectangle => build_rect_path(a, b),
        ToolType::Ellipse => build_ellipse_path(a, b),
        _ => Path::new(),
    }
}

/// Build a closed rectangle path from two opposite corners.
fn build_rect_path(a: [f64; 2], b: [f64; 2]) -> Path {
    let x0 = a[0].min(b[0]);
    let y0 = a[1].min(b[1]);
    let x1 = a[0].max(b[0]);
    let y1 = a[1].max(b[1]);

    let mut path = Path::new();
    path.subpaths.push(SubPath {
        start: Point::new(x0, y0),
        segments: vec![
            Segment::Line {
                to: Point::new(x1, y0),
            },
            Segment::Line {
                to: Point::new(x1, y1),
            },
            Segment::Line {
                to: Point::new(x0, y1),
            },
        ],
        closed: true,
        vertex_modes: vec![VertexMode::Corner; 4],
    });
    path
}

/// Build a closed ellipse path from two opposite corners of the bounding box.
///
/// Uses four arc segments (one per quadrant), which is the standard SVG
/// representation of an ellipse and preserves arc segments for round-trip
/// fidelity.
fn build_ellipse_path(a: [f64; 2], b: [f64; 2]) -> Path {
    let x0 = a[0].min(b[0]);
    let y0 = a[1].min(b[1]);
    let x1 = a[0].max(b[0]);
    let y1 = a[1].max(b[1]);

    let cx = (x0 + x1) * 0.5;
    let cy = (y0 + y1) * 0.5;
    let rx = (x1 - x0) * 0.5;
    let ry = (y1 - y0) * 0.5;

    let radii = Vec2::new(rx, ry);

    // Four quarter-arc segments, starting from the rightmost point (cx+rx, cy)
    // going clockwise: right→bottom→left→top→right
    let mut path = Path::new();
    path.subpaths.push(SubPath {
        start: Point::new(cx + rx, cy),
        segments: vec![
            // Right to bottom
            Segment::Arc {
                radii,
                x_rotation: 0.0,
                large_arc: false,
                sweep: true,
                to: Point::new(cx, cy + ry),
            },
            // Bottom to left
            Segment::Arc {
                radii,
                x_rotation: 0.0,
                large_arc: false,
                sweep: true,
                to: Point::new(cx - rx, cy),
            },
            // Left to top
            Segment::Arc {
                radii,
                x_rotation: 0.0,
                large_arc: false,
                sweep: true,
                to: Point::new(cx, cy - ry),
            },
            // Top to right (close)
            Segment::Arc {
                radii,
                x_rotation: 0.0,
                large_arc: false,
                sweep: true,
                to: Point::new(cx + rx, cy),
            },
        ],
        closed: true,
        // Ellipse arcs meet smoothly at each quadrant point.
        vertex_modes: vec![VertexMode::Smooth; 5],
    });
    path
}
