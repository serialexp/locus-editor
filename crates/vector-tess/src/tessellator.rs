use lyon_tessellation::path::Path as LyonPath;
use lyon_tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, StrokeOptions, StrokeTessellator, VertexBuffers,
};

use vector_geom::{Color, Path, Segment};

use crate::vertex::Vertex;

/// Tessellated output: vertices + indices ready for GPU upload.
pub struct TessellatedMesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

/// Describes how to paint a fill or stroke — either a solid color or a
/// reference into the gradient storage buffer.
#[derive(Debug, Clone, Copy)]
pub enum TessPaint {
    Solid(Color),
    Gradient { index: i32 },
}

/// Tessellate a path's fill and/or stroke into triangles.
pub fn tessellate_path(
    path: &Path,
    fill: Option<TessPaint>,
    stroke: Option<(TessPaint, f64)>,
) -> TessellatedMesh {
    let lyon_path = to_lyon_path(path);

    let mut buffers: VertexBuffers<Vertex, u32> = VertexBuffers::new();

    if let Some(paint) = fill {
        let mut tessellator = FillTessellator::new();
        tessellator
            .tessellate_path(
                &lyon_path,
                &FillOptions::default(),
                &mut BuffersBuilder::new(&mut buffers, |vertex: lyon_tessellation::FillVertex| {
                    let pos = [vertex.position().x, vertex.position().y];
                    match paint {
                        TessPaint::Solid(color) => {
                            Vertex::solid(pos, [color.r, color.g, color.b, color.a])
                        }
                        TessPaint::Gradient { index } => Vertex::gradient(pos, pos, index),
                    }
                }),
            )
            .expect("fill tessellation failed");
    }

    if let Some((paint, width)) = stroke {
        let mut tessellator = StrokeTessellator::new();
        tessellator
            .tessellate_path(
                &lyon_path,
                &StrokeOptions::default().with_line_width(width as f32),
                &mut BuffersBuilder::new(
                    &mut buffers,
                    |vertex: lyon_tessellation::StrokeVertex| {
                        let pos = [vertex.position().x, vertex.position().y];
                        match paint {
                            TessPaint::Solid(color) => {
                                Vertex::solid(pos, [color.r, color.g, color.b, color.a])
                            }
                            TessPaint::Gradient { index } => Vertex::gradient(pos, pos, index),
                        }
                    },
                ),
            )
            .expect("stroke tessellation failed");
    }

    TessellatedMesh {
        vertices: buffers.vertices,
        indices: buffers.indices,
    }
}

/// Convert our path representation to a lyon path.
fn to_lyon_path(path: &Path) -> LyonPath {
    let mut builder = LyonPath::builder();

    for subpath in &path.subpaths {
        builder.begin(lyon_tessellation::math::point(
            subpath.start.x as f32,
            subpath.start.y as f32,
        ));

        let mut current = subpath.start;
        for seg in &subpath.segments {
            match seg {
                Segment::Line { to } => {
                    builder.line_to(lyon_tessellation::math::point(to.x as f32, to.y as f32));
                }
                Segment::Quad { ctrl, to } => {
                    builder.quadratic_bezier_to(
                        lyon_tessellation::math::point(ctrl.x as f32, ctrl.y as f32),
                        lyon_tessellation::math::point(to.x as f32, to.y as f32),
                    );
                }
                Segment::Cubic { ctrl1, ctrl2, to } => {
                    builder.cubic_bezier_to(
                        lyon_tessellation::math::point(ctrl1.x as f32, ctrl1.y as f32),
                        lyon_tessellation::math::point(ctrl2.x as f32, ctrl2.y as f32),
                        lyon_tessellation::math::point(to.x as f32, to.y as f32),
                    );
                }
                Segment::Arc {
                    radii,
                    x_rotation,
                    large_arc,
                    sweep,
                    to,
                } => {
                    // Convert the SVG arc to cubic bezier approximations via lyon_geom.
                    let svg_arc = lyon_geom::SvgArc {
                        from: lyon_geom::point(current.x as f32, current.y as f32),
                        to: lyon_geom::point(to.x as f32, to.y as f32),
                        radii: lyon_geom::vector(radii.x as f32, radii.y as f32),
                        x_rotation: lyon_geom::Angle::radians(*x_rotation as f32),
                        flags: lyon_geom::ArcFlags {
                            large_arc: *large_arc,
                            sweep: *sweep,
                        },
                    };

                    svg_arc.for_each_cubic_bezier(&mut |cb| {
                        builder.cubic_bezier_to(
                            lyon_tessellation::math::point(cb.ctrl1.x, cb.ctrl1.y),
                            lyon_tessellation::math::point(cb.ctrl2.x, cb.ctrl2.y),
                            lyon_tessellation::math::point(cb.to.x, cb.to.y),
                        );
                    });
                }
            }
            current = seg.endpoint();
        }

        builder.end(subpath.closed);
    }

    builder.build()
}
