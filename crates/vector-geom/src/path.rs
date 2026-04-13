use serde::{Deserialize, Serialize};

use crate::{Bounds, Point, Segment};

/// A subpath: a sequence of segments starting from a given point,
/// optionally closed back to the start.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubPath {
    pub start: Point,
    pub segments: Vec<Segment>,
    pub closed: bool,
}

impl SubPath {
    pub fn new(start: Point) -> Self {
        Self {
            start,
            segments: Vec::new(),
            closed: false,
        }
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
