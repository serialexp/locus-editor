//! Small cross-module helpers: color conversion between our linear-RGBA
//! representation and egui's sRGB `Color32`, combined-bounds computation,
//! and node-display label/icon resolution.

use vector_scene::{NodeId, Scene};

/// Convert our linear RGBA `Color` to egui's sRGB `Color32`.
pub(crate) fn color_to_egui(c: vector_geom::Color) -> egui::Color32 {
    let [r, g, b, a] = c.to_srgb8();
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Convert egui's sRGB `Color32` to our linear RGBA `Color`.
pub(crate) fn egui_to_color(c: egui::Color32) -> vector_geom::Color {
    vector_geom::Color::from_srgb8(c.r(), c.g(), c.b(), c.a())
}

/// Paint a transparency checkerboard into `rect`. Used as the backdrop
/// for any colour swatch / gradient preview that may contain alpha so
/// translucency reads correctly. `cell_size` controls how large each
/// square is in screen pixels.
pub(crate) fn paint_transparency_checkerboard(
    painter: &egui::Painter,
    rect: egui::Rect,
    cell_size: f32,
) {
    let dark = egui::Color32::from_gray(120);
    let light = egui::Color32::from_gray(180);
    painter.rect_filled(rect, 2.0, light);
    let cols = (rect.width() / cell_size).ceil() as i32;
    let rows = (rect.height() / cell_size).ceil() as i32;
    for r in 0..rows {
        for col in 0..cols {
            if (r + col) % 2 == 0 {
                continue;
            }
            let x0 = rect.left() + col as f32 * cell_size;
            let y0 = rect.top() + r as f32 * cell_size;
            let x1 = (x0 + cell_size).min(rect.right());
            let y1 = (y0 + cell_size).min(rect.bottom());
            painter.rect_filled(
                egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1)),
                0.0,
                dark,
            );
        }
    }
}

/// Compute the combined world-space bounding box for a set of node IDs.
///
/// Delegates to `vector_bool::selection_visual_bounds`, which is group-aware:
/// for a regular Group it recursively unions visible descendant bounds, and
/// for a Boolean Group it uses the baked computed-path bounds. The naive
/// `NodeData::visual_bounds` returns `Bounds::EMPTY` for groups (groups have
/// no intrinsic geometry), so using it directly would hide groups from the
/// properties panel's W/H readout.
pub(crate) fn combined_bounds(scene: &Scene, node_ids: &[NodeId]) -> vector_geom::Bounds {
    vector_bool::selection_visual_bounds(scene, node_ids)
}

/// Build the display label and icon for a scene node. Used by both the
/// structure panel (virtualized tree) and the properties panel (header).
pub(crate) fn node_display(node: &vector_scene::Node) -> (&'static str, String) {
    use vector_scene::NodeData;

    let icon = match &node.data {
        NodeData::Group { is_defs: true, .. } => egui_phosphor::regular::LOCK_KEY,
        NodeData::Group {
            is_defs: false,
            kind: vector_scene::GroupKind::Boolean { .. },
        } => egui_phosphor::regular::INTERSECT_SQUARE,
        NodeData::Group { is_defs: false, .. } => egui_phosphor::regular::FOLDER,
        NodeData::Path { .. } => egui_phosphor::regular::PATH,
        NodeData::Paint(_) => egui_phosphor::regular::PALETTE,
        NodeData::Text(_) => egui_phosphor::regular::TEXT_AA,
        NodeData::Raster { .. } => egui_phosphor::regular::IMAGE,
    };

    let label = if node.label.is_empty() {
        match &node.data {
            NodeData::Group { is_defs: true, .. } => "defs".to_string(),
            NodeData::Group { .. } => "Group".to_string(),
            NodeData::Path { .. } => "Path".to_string(),
            NodeData::Paint(_) => "Paint".to_string(),
            NodeData::Text(t) => {
                let preview: String = t.content.chars().take(20).collect();
                if t.content.len() > 20 {
                    format!("{preview}...")
                } else {
                    preview
                }
            }
            NodeData::Raster { .. } => "Image".to_string(),
        }
    } else {
        node.label.clone()
    };

    (icon, label)
}
