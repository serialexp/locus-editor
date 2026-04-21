//! Raster-to-vector conversion (image tracing).
//!
//! Wraps the [`vtracer`] library. We decode the raster with [`image`] into
//! an RGBA8 buffer, hand it to vtracer as a [`visioncortex::ColorImage`],
//! and then re-parse vtracer's generated SVG through our own
//! [`vector_svg::import_svg`] pipeline. That path reuse is deliberate: all
//! segment conversion, color handling and smooth-junction detection already
//! lives in `vector-svg`, and going through SVG as the interchange format
//! avoids duplicating any of it.
//!
//! This crate intentionally does *not* retain the source raster — tracing
//! is a one-shot import. If we later want round-trippable "traceable image
//! nodes", that would be a separate feature layered on top.

use std::io::Cursor;

use vector_scene::Scene;
use vector_svg::ImportError;

pub use vtracer::{ColorMode, Hierarchical};

/// Quality/style preset for the tracer. Mirrors vtracer's presets, plus a
/// `Custom` escape hatch for callers that want full control.
#[derive(Debug, Clone, Default)]
pub enum TracePreset {
    /// Bi-level (black & white) tracing. Best for line art, logos, scans.
    Bw,
    /// Posterized color tracing. Best for illustrations and flat-color art.
    #[default]
    Poster,
    /// Photo tracing — more layers, larger speckle filter.
    Photo,
    /// Caller-supplied vtracer config.
    Custom(Box<vtracer::Config>),
}

impl TracePreset {
    fn into_config(self) -> vtracer::Config {
        match self {
            TracePreset::Bw => vtracer::Config::from_preset(vtracer::Preset::Bw),
            TracePreset::Poster => vtracer::Config::from_preset(vtracer::Preset::Poster),
            TracePreset::Photo => vtracer::Config::from_preset(vtracer::Preset::Photo),
            TracePreset::Custom(cfg) => *cfg,
        }
    }
}

/// Trace a raster image (bytes of a PNG/JPEG/GIF/BMP/WEBP/TIFF) into a
/// fresh [`Scene`].
///
/// The returned scene contains one `Path` node per traced region, stacked
/// in the order vtracer produces them, with solid-color fills.
pub fn trace_image_bytes(bytes: &[u8], preset: TracePreset) -> Result<Scene, TraceError> {
    // Decode into RGBA8 with the modern `image` crate — we don't rely on
    // vtracer's transitive `image` dep, which is pinned to an old major.
    let decoded = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(TraceError::Io)?
        .decode()
        .map_err(TraceError::Decode)?
        .to_rgba8();

    let (width, height) = (decoded.width() as usize, decoded.height() as usize);
    if width == 0 || height == 0 {
        return Err(TraceError::EmptyImage);
    }

    // vtracer re-exports `visioncortex::ColorImage`. Its layout is exactly
    // `Vec<u8>` of R,G,B,A bytes in row-major order — identical to
    // `image::RgbaImage::as_raw()`.
    let color_image = vtracer::ColorImage {
        pixels: decoded.into_raw(),
        width,
        height,
    };

    let svg_file =
        vtracer::convert(color_image, preset.into_config()).map_err(TraceError::Vtracer)?;

    // `SvgFile: Display` produces a complete, self-contained SVG document.
    let svg_text = svg_file.to_string();

    // Hand it off to our SVG importer, reusing all path/color machinery.
    vector_svg::import_svg(svg_text.as_bytes()).map_err(TraceError::Svg)
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("failed to read image bytes: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to decode image: {0}")]
    Decode(image::ImageError),
    #[error("empty image (zero width or height)")]
    EmptyImage,
    #[error("vtracer error: {0}")]
    Vtracer(String),
    #[error("re-parsing traced SVG failed: {0}")]
    Svg(ImportError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_scene::NodeData;

    /// Build a tiny 4×4 PNG with two colored quadrants and confirm that
    /// tracing produces at least one path node with a solid fill.
    #[test]
    fn trace_simple_png_produces_paths() {
        // Build a 16×16 image: top-left red, rest white. vtracer needs a few
        // pixels of margin to find a region, so we don't go smaller.
        let mut img = image::RgbaImage::new(16, 16);
        for y in 0..16 {
            for x in 0..16 {
                let p = if x < 8 && y < 8 {
                    image::Rgba([255, 0, 0, 255])
                } else {
                    image::Rgba([255, 255, 255, 255])
                };
                img.put_pixel(x, y, p);
            }
        }
        let mut png_bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();

        let scene =
            trace_image_bytes(&png_bytes, TracePreset::Poster).expect("tracing should succeed");

        let root = scene.root();
        let root_node = scene.get(root).unwrap();
        let path_count = root_node
            .children
            .iter()
            .filter(|&&id| {
                scene
                    .get(id)
                    .is_some_and(|n| matches!(n.data, NodeData::Path { .. }))
            })
            .count();
        assert!(
            path_count >= 1,
            "expected at least one traced path, got {path_count}"
        );
    }

    #[test]
    fn trace_rejects_invalid_bytes() {
        let result = trace_image_bytes(b"not an image", TracePreset::Poster);
        assert!(result.is_err());
    }
}
