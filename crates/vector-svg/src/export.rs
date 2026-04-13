use vector_scene::Scene;

/// Export a scene graph to an SVG string.
pub fn export_svg(scene: &Scene) -> String {
    // TODO: walk the scene tree and emit SVG XML.
    // For now, produce a minimal valid SVG.
    let _ = scene;
    String::from(r#"<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 800 600">
</svg>"#)
}
