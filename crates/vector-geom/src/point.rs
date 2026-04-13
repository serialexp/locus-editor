use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

/// A 2D point in document space.
#[derive(Debug, Clone, Copy, PartialEq, Default, Pod, Zeroable, Serialize, Deserialize)]
#[repr(C)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Vector from self to other.
    pub fn to(self, other: Self) -> Vec2 {
        Vec2 {
            x: other.x - self.x,
            y: other.y - self.y,
        }
    }

    /// Distance to another point.
    pub fn distance(self, other: Self) -> f64 {
        let dx = other.x - self.x;
        let dy = other.y - self.y;
        (dx * dx + dy * dy).sqrt()
    }

    /// Translate by a vector.
    pub fn translate(self, v: Vec2) -> Self {
        Self {
            x: self.x + v.x,
            y: self.y + v.y,
        }
    }
}

/// A 2D vector (direction + magnitude), distinct from a point.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    pub fn length(self) -> f64 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    pub fn normalize(self) -> Self {
        let len = self.length();
        if len < 1e-12 {
            Self::ZERO
        } else {
            Self {
                x: self.x / len,
                y: self.y / len,
            }
        }
    }

    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// 2D cross product (z-component of the 3D cross product).
    pub fn cross(self, other: Self) -> f64 {
        self.x * other.y - self.y * other.x
    }

    pub fn scale(self, s: f64) -> Self {
        Self {
            x: self.x * s,
            y: self.y * s,
        }
    }
}

impl From<Point> for kurbo::Point {
    fn from(p: Point) -> Self {
        kurbo::Point::new(p.x, p.y)
    }
}

impl From<kurbo::Point> for Point {
    fn from(p: kurbo::Point) -> Self {
        Point::new(p.x, p.y)
    }
}
