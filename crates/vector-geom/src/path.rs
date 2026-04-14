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
}
