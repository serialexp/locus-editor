use vector_geom::{Color, Path, Point, Segment, SubPath};
use vector_scene::{NodeData, Scene, Style};
use vector_scene::Node;

/// Import an SVG file from bytes into a scene graph.
pub fn import_svg(data: &[u8]) -> Result<Scene, ImportError> {
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &opts).map_err(ImportError::Parse)?;

    let mut scene = Scene::new();
    let root = scene.root();

    import_group(&tree.root(), &mut scene, root);

    Ok(scene)
}

fn import_group(group: &usvg::Group, scene: &mut Scene, parent: vector_scene::NodeId) {
    for child in group.children() {
        match child {
            usvg::Node::Group(g) => {
                let node = Node::group(g.id().to_string());
                if let Some(id) = scene.insert(parent, node) {
                    import_group(g, scene, id);
                }
            }
            usvg::Node::Path(p) => {
                let path = convert_path(p.data());
                let style = convert_style(p);
                let mut node = Node::path(p.id().to_string(), path);
                if let NodeData::Path {
                    style: s, ..
                } = &mut node.data
                {
                    *s = style;
                }
                scene.insert(parent, node);
            }
            usvg::Node::Image(_) => {
                log::warn!("Image nodes not yet supported, skipping");
            }
            usvg::Node::Text(t) => {
                log::warn!("Text node '{}' not yet fully supported, importing as group", t.id());
            }
        }
    }
}

fn convert_path(data: &usvg::tiny_skia_path::Path) -> Path {
    let mut path = Path::new();
    let mut current_subpath: Option<SubPath> = None;

    for seg in data.segments() {
        match seg {
            usvg::tiny_skia_path::PathSegment::MoveTo(pt) => {
                if let Some(sp) = current_subpath.take() {
                    path.subpaths.push(sp);
                }
                current_subpath = Some(SubPath::new(Point::new(pt.x as f64, pt.y as f64)));
            }
            usvg::tiny_skia_path::PathSegment::LineTo(pt) => {
                if let Some(sp) = &mut current_subpath {
                    sp.segments.push(Segment::Line {
                        to: Point::new(pt.x as f64, pt.y as f64),
                    });
                }
            }
            usvg::tiny_skia_path::PathSegment::QuadTo(ctrl, to) => {
                if let Some(sp) = &mut current_subpath {
                    sp.segments.push(Segment::Quad {
                        ctrl: Point::new(ctrl.x as f64, ctrl.y as f64),
                        to: Point::new(to.x as f64, to.y as f64),
                    });
                }
            }
            usvg::tiny_skia_path::PathSegment::CubicTo(ctrl1, ctrl2, to) => {
                if let Some(sp) = &mut current_subpath {
                    sp.segments.push(Segment::Cubic {
                        ctrl1: Point::new(ctrl1.x as f64, ctrl1.y as f64),
                        ctrl2: Point::new(ctrl2.x as f64, ctrl2.y as f64),
                        to: Point::new(to.x as f64, to.y as f64),
                    });
                }
            }
            usvg::tiny_skia_path::PathSegment::Close => {
                if let Some(sp) = &mut current_subpath {
                    sp.closed = true;
                }
                if let Some(sp) = current_subpath.take() {
                    path.subpaths.push(sp);
                }
            }
        }
    }

    if let Some(sp) = current_subpath {
        path.subpaths.push(sp);
    }

    path
}

fn convert_style(path: &usvg::Path) -> Style {
    let fill = path.fill().map(|f| {
        let paint = match f.paint() {
            usvg::Paint::Color(c) => {
                vector_scene::PaintRef::Solid(Color::from_srgb8(c.red, c.green, c.blue, 255))
            }
            _ => {
                log::warn!("Non-solid fill paints not yet imported, falling back to black");
                vector_scene::PaintRef::Solid(Color::BLACK)
            }
        };
        vector_scene::style::Fill {
            paint,
            rule: match f.rule() {
                usvg::FillRule::NonZero => vector_scene::FillRule::NonZero,
                usvg::FillRule::EvenOdd => vector_scene::FillRule::EvenOdd,
            },
            opacity: f.opacity().get(),
        }
    });

    let stroke = path.stroke().map(|s| {
        let paint = match s.paint() {
            usvg::Paint::Color(c) => {
                vector_scene::PaintRef::Solid(Color::from_srgb8(c.red, c.green, c.blue, 255))
            }
            _ => {
                log::warn!("Non-solid stroke paints not yet imported, falling back to black");
                vector_scene::PaintRef::Solid(Color::BLACK)
            }
        };
        vector_scene::style::Stroke {
            paint,
            style: vector_scene::StrokeStyle {
                width: s.width().get() as f64,
                cap: match s.linecap() {
                    usvg::LineCap::Butt => vector_scene::LineCap::Butt,
                    usvg::LineCap::Round => vector_scene::LineCap::Round,
                    usvg::LineCap::Square => vector_scene::LineCap::Square,
                },
                join: match s.linejoin() {
                    usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => {
                        vector_scene::LineJoin::Miter
                    }
                    usvg::LineJoin::Round => vector_scene::LineJoin::Round,
                    usvg::LineJoin::Bevel => vector_scene::LineJoin::Bevel,
                },
                miter_limit: s.miterlimit().get() as f64,
                dash: s.dasharray().as_ref().map(|d| vector_scene::style::DashPattern {
                    array: d.iter().map(|v| *v as f64).collect(),
                    offset: s.dashoffset() as f64,
                }),
            },
            opacity: s.opacity().get(),
        }
    });

    Style { fill, stroke }
}

#[derive(Debug)]
pub enum ImportError {
    Parse(usvg::Error),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Parse(e) => write!(f, "SVG parse error: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}
