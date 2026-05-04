// Interactive editor tools.
// Each tool is a state machine that consumes input events
// and emits Commands (via vector-ops) to modify the scene.

mod pen;
mod select;
mod shape;
mod text;

pub use pen::{PenAction, PenState};
pub use select::{
    EdgeHit, GradientHandlePoint, GradientHandleRef, PointKind, ScaleHandle, SegmentKind,
    SelectState, SelectionMode, VertexRef, for_each_handle_of_gradient,
};
pub use shape::ShapeDrawState;
pub use text::{TextAction, TextToolState};

/// The available editing tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ToolType {
    #[default]
    Select,
    Pen,
    Rectangle,
    Ellipse,
    Text,
}

impl ToolType {
    /// All tool types in toolbar order.
    pub const ALL: &[ToolType] = &[
        ToolType::Select,
        ToolType::Pen,
        ToolType::Rectangle,
        ToolType::Ellipse,
        ToolType::Text,
    ];

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            ToolType::Select => "Select",
            ToolType::Pen => "Pen",
            ToolType::Rectangle => "Rectangle",
            ToolType::Ellipse => "Ellipse",
            ToolType::Text => "Text",
        }
    }

    /// Phosphor icon constant (egui_phosphor::regular).
    pub fn icon(self) -> &'static str {
        match self {
            ToolType::Select => egui_phosphor::regular::CURSOR,
            ToolType::Pen => egui_phosphor::regular::PEN_NIB,
            ToolType::Rectangle => egui_phosphor::regular::RECTANGLE,
            ToolType::Ellipse => egui_phosphor::regular::CIRCLE,
            ToolType::Text => egui_phosphor::regular::TEXT_AA,
        }
    }
}
