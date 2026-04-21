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

/// Compute the combined world-space bounding box for a set of node IDs.
pub(crate) fn combined_bounds(scene: &Scene, node_ids: &[NodeId]) -> vector_geom::Bounds {
    let mut combined = vector_geom::Bounds::EMPTY;
    for &id in node_ids {
        if let Some(node) = scene.get(id) {
            let world = scene.world_transform(id);
            let b = node.data.visual_bounds(world);
            if !b.is_empty() {
                combined = combined.union(b);
            }
        }
    }
    combined
}

/// Build the display label and icon for a scene node. Used by both the
/// structure panel (virtualized tree) and the properties panel (header).
pub(crate) fn node_display(node: &vector_scene::Node) -> (&'static str, String) {
    use vector_scene::NodeData;

    let icon = match &node.data {
        NodeData::Group { is_defs: true } => egui_phosphor::regular::LOCK_KEY,
        NodeData::Group { is_defs: false } => egui_phosphor::regular::FOLDER,
        NodeData::Path { .. } => egui_phosphor::regular::PATH,
        NodeData::Paint(_) => egui_phosphor::regular::PALETTE,
        NodeData::Text(_) => egui_phosphor::regular::TEXT_AA,
    };

    let label = if node.label.is_empty() {
        match &node.data {
            NodeData::Group { is_defs: true } => "defs".to_string(),
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
        }
    } else {
        node.label.clone()
    };

    (icon, label)
}
