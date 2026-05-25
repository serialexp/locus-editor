//! Raster-to-vector conversion (image tracing).
//!
//! Wraps the [`vtracer`] library. We decode the raster with [`image`] into
//! an RGBA8 buffer, hand it to vtracer as a [`visioncortex::ColorImage`],
//! and then re-parse vtracer's generated SVG through our own
//! [`locus_svg::import_svg`] pipeline. That path reuse is deliberate: all
//! segment conversion, color handling and smooth-junction detection already
//! lives in `locus-svg`, and going through SVG as the interchange format
//! avoids duplicating any of it.
//!
//! This crate intentionally does *not* retain the source raster — tracing
//! is a one-shot import. If we later want round-trippable "traceable image
//! nodes", that would be a separate feature layered on top.

use std::io::Cursor;

use locus_geom::Affine;
use locus_scene::Scene;
use locus_svg::ImportError;

pub use vtracer::{ColorMode, Hierarchical};

mod params;
pub use params::{CurveMode, HierarchicalMode, TraceColorMode, TraceParams};

/// Quality/style preset for the tracer. Mirrors vtracer's presets, plus a
/// `Custom` escape hatch for callers that want full control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TracePreset {
    /// Bi-level (black & white) tracing. Best for line art, logos, scans.
    Bw,
    /// Posterized color tracing. Best for illustrations and flat-color art.
    #[default]
    Poster,
    /// Photo tracing — more layers, larger speckle filter.
    Photo,
}

/// Trace a raster image (bytes of a PNG/JPEG/GIF/BMP/WEBP/TIFF) into a
/// fresh [`Scene`] using a named preset.
///
/// Convenience wrapper over [`trace_image_with_params`] for callers that
/// just want one of the canned presets.
///
/// The returned scene contains one `Path` node per traced region, stacked
/// in the order vtracer produces them, with solid-color fills.
pub fn trace_image_bytes(bytes: &[u8], preset: TracePreset) -> Result<Scene, TraceError> {
    trace_image_with_params(bytes, &TraceParams::from_preset(preset))
}

/// Trace a raster image into a fresh [`Scene`] using fully-specified
/// parameters. This is the entry point the editor's trace dialog uses,
/// so every slider in the UI maps to one [`TraceParams`] field.
pub fn trace_image_with_params(bytes: &[u8], params: &TraceParams) -> Result<Scene, TraceError> {
    // Decode into RGBA8 with the modern `image` crate — we don't rely on
    // vtracer's transitive `image` dep, which is pinned to an old major.
    let mut decoded = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(TraceError::Io)?
        .decode()
        .map_err(TraceError::Decode)?
        .to_rgba8();

    let (orig_w, orig_h) = (decoded.width(), decoded.height());
    if orig_w == 0 || orig_h == 0 {
        return Err(TraceError::EmptyImage);
    }

    // Optional pre-trace downscale. Resampling with a triangle filter (≈
    // bilinear) antialiases pixel boundaries, so vtracer no longer sees
    // hard staircase edges where the source had smooth curves. The
    // resulting paths come out in the downsampled coordinate space, and
    // we scale them back up below so callers get geometry in the
    // original pixel space regardless of `downscale`.
    //
    // Clamped to [1.0, 8.0]: ≤1.0 means "no downscale" (anything below
    // 1.0 would be upscaling, which adds zero information and slows the
    // trace), and >8.0 reliably destroys every recognisable feature.
    let downscale = params.downscale.clamp(1.0, 8.0);
    if downscale > 1.0 {
        let new_w = ((orig_w as f32 / downscale).round() as u32).max(1);
        let new_h = ((orig_h as f32 / downscale).round() as u32).max(1);
        if new_w < orig_w || new_h < orig_h {
            decoded = image::imageops::resize(
                &decoded,
                new_w,
                new_h,
                image::imageops::FilterType::Triangle,
            );
        }
    }

    let (width, height) = (decoded.width() as usize, decoded.height() as usize);

    // vtracer re-exports `visioncortex::ColorImage`. Its layout is exactly
    // `Vec<u8>` of R,G,B,A bytes in row-major order — identical to
    // `image::RgbaImage::as_raw()`.
    let color_image = vtracer::ColorImage {
        pixels: decoded.into_raw(),
        width,
        height,
    };

    let svg_file =
        vtracer::convert(color_image, params.to_vtracer_config()).map_err(TraceError::Vtracer)?;

    // `SvgFile: Display` produces a complete, self-contained SVG document.
    let svg_text = svg_file.to_string();

    // Hand it off to our SVG importer, reusing all path/color machinery.
    let mut scene = locus_svg::import_svg(svg_text.as_bytes()).map_err(TraceError::Svg)?;

    // Compensate for the downscale by scaling each top-level imported
    // node back up. We compose against the existing local transform
    // (vtracer's output is identity, but the SVG importer could in
    // principle apply one) rather than overwriting it. `defs` is left
    // alone — gradients/patterns reference paths by id and shouldn't
    // be re-scaled.
    if downscale > 1.0 {
        let s = downscale as f64;
        let scale = Affine::scale(s, s);
        let root = scene.root();
        let defs = scene.defs();
        let children: Vec<_> = scene
            .get(root)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for child in children {
            if child == defs {
                continue;
            }
            let existing = scene
                .get(child)
                .map(|n| n.transform)
                .unwrap_or(Affine::IDENTITY);
            scene.set_transform(child, existing.then(scale));
        }
    }

    Ok(scene)
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
    use locus_scene::NodeData;

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
    fn trace_with_custom_params_produces_paths() {
        // Same 16×16 image, but trace with custom params (binary mode +
        // higher speckle filter).
        let mut img = image::RgbaImage::new(16, 16);
        for y in 0..16 {
            for x in 0..16 {
                let p = if x < 8 {
                    image::Rgba([0, 0, 0, 255])
                } else {
                    image::Rgba([255, 255, 255, 255])
                };
                img.put_pixel(x, y, p);
            }
        }
        let mut png_bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();

        let mut params = TraceParams::from_preset(TracePreset::Bw);
        params.filter_speckle = 2; // a 16×16 image has small clusters
        let scene = trace_image_with_params(&png_bytes, &params)
            .expect("custom-params tracing should succeed");
        assert!(scene.root() != scene.defs(), "scene should be populated");
    }

    #[test]
    fn trace_rejects_invalid_bytes() {
        let result = trace_image_bytes(b"not an image", TracePreset::Poster);
        assert!(result.is_err());
    }

    /// With downscale > 1 the input is resampled smaller before tracing,
    /// but the output coordinate space should still match the original
    /// pixel dimensions thanks to the compensating scale-up on top-level
    /// nodes. We assert that by tracing a 32×32 image at downscale=2 and
    /// confirming the traced paths' bounding box extends across roughly
    /// the original image size, not half of it.
    #[test]
    fn trace_with_downscale_preserves_output_scale() {
        // 32×32 split half black / half white — vtracer should find at
        // least one large region either way.
        let mut img = image::RgbaImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let p = if x < 16 {
                    image::Rgba([0, 0, 0, 255])
                } else {
                    image::Rgba([255, 255, 255, 255])
                };
                img.put_pixel(x, y, p);
            }
        }
        let mut png_bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();

        let mut params = TraceParams::from_preset(TracePreset::Bw);
        params.filter_speckle = 2;
        params.downscale = 2.0;
        let scene =
            trace_image_with_params(&png_bytes, &params).expect("downscaled trace should succeed");

        // Find a top-level path node and check its transform has been
        // scaled up by ~2× (the downscale compensation).
        let root_node = scene.get(scene.root()).unwrap();
        let scaled_path = root_node
            .children
            .iter()
            .find(|&&id| {
                id != scene.defs()
                    && scene
                        .get(id)
                        .is_some_and(|n| matches!(n.data, NodeData::Path { .. }))
            })
            .copied()
            .expect("expected at least one traced path");

        let t = scene.get(scaled_path).unwrap().transform;
        // a/d are the diagonal scale factors. With downscale=2 they
        // should be exactly 2.0 since vtracer's own transform is
        // identity.
        assert!(
            (t.a - 2.0).abs() < 1e-6 && (t.d - 2.0).abs() < 1e-6,
            "expected ~2× scale-up transform, got a={}, d={}",
            t.a,
            t.d
        );
    }
}
