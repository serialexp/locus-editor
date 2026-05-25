pub mod merge;
pub mod node;
pub mod paint;
pub mod scene;
pub mod style;

pub use merge::merge_as_group;
pub use node::{BoolOp, GroupKind, Node, NodeData, RasterImage, TextData};
pub use paint::{
    ColorStop, Gradient, GradientKind, InterpolationSpace, Paint, PaintRef, Pattern, SpreadMethod,
};
pub use scene::{NodeEditGuard, NodeId, NodeRevision, NodeSnapshot, Scene};
pub use style::{Fill, FillRule, LineCap, LineJoin, Stroke, StrokeStyle, Style};
