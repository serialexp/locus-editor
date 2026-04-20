use serde::{Deserialize, Serialize};

use crate::Point;

/// A 2D affine transform stored as a 3×2 matrix (row-major):
///
/// ```text
/// | a  b  tx |
/// | c  d  ty |
/// | 0  0   1 |
/// ```
///
/// Transforms a point as: x' = a*x + b*y + tx, y' = c*x + d*y + ty
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Affine {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub fn translate(tx: f64, ty: f64) -> Self {
        Self {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            tx,
            ty,
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Self {
        Self {
            a: sx,
            b: 0.0,
            c: 0.0,
            d: sy,
            tx: 0.0,
            ty: 0.0,
        }
    }

    pub fn rotate(angle_rad: f64) -> Self {
        let (s, c) = angle_rad.sin_cos();
        Self {
            a: c,
            b: -s,
            c: s,
            d: c,
            tx: 0.0,
            ty: 0.0,
        }
    }

    /// Compose two transforms: self * other (self applied after other).
    pub fn then(self, other: Self) -> Self {
        Self {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            tx: self.a * other.tx + self.b * other.ty + self.tx,
            ty: self.c * other.tx + self.d * other.ty + self.ty,
        }
    }

    /// Transform a point.
    pub fn apply(self, p: Point) -> Point {
        Point::new(
            self.a * p.x + self.b * p.y + self.tx,
            self.c * p.x + self.d * p.y + self.ty,
        )
    }

    /// Determinant of the linear part.
    pub fn determinant(self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    /// Inverse transform, if it exists (non-zero determinant).
    pub fn inverse(self) -> Option<Self> {
        let det = self.determinant();
        if det.abs() < 1e-12 {
            return None;
        }
        let inv_det = 1.0 / det;
        Some(Self {
            a: self.d * inv_det,
            b: -self.b * inv_det,
            c: -self.c * inv_det,
            d: self.a * inv_det,
            tx: (self.b * self.ty - self.d * self.tx) * inv_det,
            ty: (self.c * self.tx - self.a * self.ty) * inv_det,
        })
    }

    pub fn is_identity(self) -> bool {
        self == Self::IDENTITY
    }

    /// Extract the rotation angle (in radians) from the linear part of this
    /// affine. For a pure translate+rotate+uniform-scale transform, this
    /// returns the exact angle. For non-uniform scale or skew, this returns
    /// the angle of the first basis vector (the column [a, c]).
    pub fn rotation_rad(self) -> f64 {
        self.c.atan2(self.a)
    }

    /// Extract the rotation angle in degrees.
    pub fn rotation_deg(self) -> f64 {
        self.rotation_rad().to_degrees()
    }

    /// Extract the scale factors (sx, sy) from the linear part.
    /// For a transform composed as Scale * Rotate, this returns the
    /// magnitudes of the two basis vectors.
    pub fn scale_factors(self) -> (f64, f64) {
        let sx = (self.a * self.a + self.c * self.c).sqrt();
        let sy = (self.b * self.b + self.d * self.d).sqrt();
        // Preserve sign via determinant — negative det means a flip.
        let sign = if self.determinant() < 0.0 { -1.0 } else { 1.0 };
        (sx, sy * sign)
    }

    /// Build a transform that rotates `angle_rad` around a given center point.
    pub fn rotate_around(angle_rad: f64, center: Point) -> Self {
        Self::translate(center.x, center.y)
            .then(Self::rotate(angle_rad))
            .then(Self::translate(-center.x, -center.y))
    }
}

impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl From<Affine> for kurbo::Affine {
    fn from(a: Affine) -> Self {
        kurbo::Affine::new([a.a, a.c, a.b, a.d, a.tx, a.ty])
    }
}

impl From<kurbo::Affine> for Affine {
    fn from(k: kurbo::Affine) -> Self {
        let c = k.as_coeffs();
        Affine {
            a: c[0],
            b: c[2],
            c: c[1],
            d: c[3],
            tx: c[4],
            ty: c[5],
        }
    }
}
