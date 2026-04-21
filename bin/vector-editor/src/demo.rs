//! Startup demo content — a lone triangle so new windows aren't blank.

use vector_scene::Scene;

pub(crate) fn create_demo_content(scene: &mut Scene) {
    use vector_geom::*;

    // A simple triangle path
    let mut path = Path::new();
    path.subpaths.push(SubPath {
        start: Point::new(400.0, 100.0),
        segments: vec![
            Segment::Line {
                to: Point::new(600.0, 400.0),
            },
            Segment::Line {
                to: Point::new(200.0, 400.0),
            },
        ],
        closed: true,
        vertex_modes: vec![VertexMode::Corner; 3],
    });

    let mut node = vector_scene::Node::path("demo triangle", path);
    if let vector_scene::NodeData::Path { style, .. } = &mut node.data {
        style.fill = Some(vector_scene::style::Fill {
            paint: vector_scene::PaintRef::Solid(Color::from_srgb8(70, 130, 180, 255)),
            rule: vector_scene::FillRule::NonZero,
            opacity: 1.0,
        });
    }

    let root = scene.root();
    scene.insert(root, node);
}
