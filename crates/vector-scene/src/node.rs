use serde::{Deserialize, Serialize};
use vector_geom::{Affine, Path};

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

    /// A paint definition (gradient). Lives in the defs subtree
    /// and is referenced via `PaintRef::Ref`.
    Paint(Paint),

    /// Editable text. The rendered glyph outlines are computed on demand
    /// (by vector-text) and cached — not stored here.
    Text(TextData),
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
