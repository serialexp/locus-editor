use serde::{Deserialize, Serialize};

use crate::Point;

/// Axis-aligned bounding box.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Bounds {
    pub min: Point,
    pub max: Point,
}

impl Bounds {
    pub const EMPTY: Self = Self {
        min: Point::new(f64::INFINITY, f64::INFINITY),
        max: Point::new(f64::NEG_INFINITY, f64::NEG_INFINITY),
    };

    pub fn new(min: Point, max: Point) -> Self {
        Self { min, max }
    }

    pub fn from_points(points: impl IntoIterator<Item = Point>) -> Self {
        let mut b = Self::EMPTY;
        for p in points {
            b = b.include_point(p);
        }
        b
    }

    pub fn include_point(self, p: Point) -> Self {
        Self {
            min: Point::new(self.min.x.min(p.x), self.min.y.min(p.y)),
            max: Point::new(self.max.x.max(p.x), self.max.y.max(p.y)),
        }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min: Point::new(self.min.x.min(other.min.x), self.min.y.min(other.min.y)),
            max: Point::new(self.max.x.max(other.max.x), self.max.y.max(other.max.y)),
        }
    }

    pub fn intersects(self, other: Self) -> bool {
        self.min.x <= other.max.x
            && self.max.x >= other.min.x
            && self.min.y <= other.max.y
            && self.max.y >= other.min.y
    }

    pub fn contains_point(self, p: Point) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }

    pub fn width(self) -> f64 {
        (self.max.x - self.min.x).max(0.0)
    }

    pub fn height(self) -> f64 {
        (self.max.y - self.min.y).max(0.0)
    }

    pub fn center(self) -> Point {
        Point::new(
            (self.min.x + self.max.x) * 0.5,
            (self.min.y + self.max.y) * 0.5,
        )
    }

    pub fn is_empty(self) -> bool {
        self.min.x > self.max.x || self.min.y > self.max.y
    }

    /// Transform this bounding box by an affine transform, returning a new
    /// axis-aligned bounding box that encloses the transformed corners.
    pub fn transform(self, xform: crate::Affine) -> Self {
        if self.is_empty() {
            return self;
        }
        let corners = [
            xform.apply(self.min),
            xform.apply(Point::new(self.max.x, self.min.y)),
            xform.apply(self.max),
            xform.apply(Point::new(self.min.x, self.max.y)),
        ];
        Self::from_points(corners)
    }
}

impl Default for Bounds {
    fn default() -> Self {
        Self::EMPTY
    }
}
