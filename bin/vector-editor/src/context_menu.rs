//! Canvas right-click context menu — actions on the vertex or segment
//! that was clicked (set vertex mode, delete vertex, convert segment,
//! insert vertex).

use vector_ops::Command;
use vector_render::Renderer;
use vector_scene::NodeData;
use vector_tools::{EdgeHit, PointKind, SelectState, VertexRef};

use crate::editor_state::{CanvasContextMenu, CanvasContextTarget, EditorState};

/// Render the canvas context menu (right-click on vertex/segment).
pub(crate) fn show_canvas_context_menu(
    ctx: &egui::Context,
    state: &mut EditorState,
    renderer: &mut Renderer,
) {
    let Some(menu) = &mut state.canvas_context_menu else {
        return;
    };

    // We need an Area so the menu floats over the canvas. On the first
    // frame we position it; on subsequent frames egui remembers.
    let area_id = egui::Id::new("canvas_context_menu");
    let pos = menu.screen_pos;
    if menu.first_frame {
        menu.first_frame = false;
    }

    let mut close = false;
    let mut action: Option<CanvasContextAction> = None;

    let area_resp = egui::Area::new(area_id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            let frame = egui::Frame::popup(ui.style());
            frame.show(ui, |ui| {
                match &state.canvas_context_menu {
                    Some(CanvasContextMenu {
                        target: CanvasContextTarget::Vertex(vr),
                        ..
                    }) => {
                        let vr = *vr;
                        let is_anchor =
                            matches!(vr.kind, PointKind::SubpathStart | PointKind::Endpoint);

                        ui.label(
                            egui::RichText::new(if is_anchor { "Vertex" } else { "Control Point" })
                                .strong(),
                        );
                        ui.separator();

                        if is_anchor {
                            // Vertex mode buttons
                            use vector_geom::VertexMode;
                            let current_mode = state.scene.get(vr.node).and_then(|n| {
                                if let NodeData::Path { ref path, .. } = n.data {
                                    let sp = path.subpaths.get(vr.subpath)?;
                                    let idx = match vr.kind {
                                        PointKind::SubpathStart => 0,
                                        PointKind::Endpoint => vr.segment + 1,
                                        _ => return None,
                                    };
                                    sp.vertex_modes.get(idx).copied()
                                } else {
                                    None
                                }
                            });

                            if let Some(current) = current_mode {
                                for (mode, label) in [
                                    (VertexMode::Corner, "Corner"),
                                    (VertexMode::Smooth, "Smooth"),
                                    (VertexMode::Symmetric, "Symmetric"),
                                ] {
                                    let is_active = current == mode;
                                    if ui.selectable_label(is_active, label).clicked() && !is_active
                                    {
                                        action = Some(CanvasContextAction::SetVertexMode(vr, mode));
                                        close = true;
                                    }
                                }
                                ui.separator();
                            }

                            if ui.button("Delete Vertex").clicked() {
                                action = Some(CanvasContextAction::DeleteVertex(vr));
                                close = true;
                            }
                        }
                    }
                    Some(CanvasContextMenu {
                        target: CanvasContextTarget::Segment(hit),
                        ..
                    }) => {
                        let hit = hit.clone();
                        let seg_kind = SelectState::segment_type(&state.scene, &hit);

                        ui.label(egui::RichText::new("Segment").strong());
                        ui.separator();

                        if let Some(kind) = seg_kind {
                            use vector_tools::SegmentKind;
                            for (target, label) in [
                                (SegmentKind::Line, "Line"),
                                (SegmentKind::Quad, "Quadratic"),
                                (SegmentKind::Cubic, "Cubic"),
                            ] {
                                let is_active = kind == target;
                                if ui.selectable_label(is_active, label).clicked() && !is_active {
                                    action = Some(CanvasContextAction::ConvertSegment(
                                        hit.clone(),
                                        target,
                                    ));
                                    close = true;
                                }
                            }
                            ui.separator();
                        }

                        if ui.button("Insert Vertex").clicked() {
                            action = Some(CanvasContextAction::InsertVertex(hit));
                            close = true;
                        }
                    }
                    None => {}
                }
            });
        });

    // Close the menu if the user clicks anywhere outside it.
    let menu_rect = area_resp.response.rect;
    if ctx.input(|i| i.pointer.any_pressed()) {
        let pointer_over_menu = ctx
            .input(|i| i.pointer.hover_pos())
            .is_some_and(|p| menu_rect.contains(p));
        if !pointer_over_menu {
            close = true;
        }
    }

    // Dismiss on Escape.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        close = true;
    }

    // Apply the action after borrowing is released.
    if let Some(action) = action {
        apply_canvas_context_action(state, renderer, action);
    }

    if close {
        state.canvas_context_menu = None;
    }
}

/// Deferred action from a canvas context menu click.
enum CanvasContextAction {
    SetVertexMode(VertexRef, vector_geom::VertexMode),
    DeleteVertex(VertexRef),
    ConvertSegment(EdgeHit, vector_tools::SegmentKind),
    InsertVertex(EdgeHit),
}

fn apply_canvas_context_action(
    state: &mut EditorState,
    renderer: &mut Renderer,
    action: CanvasContextAction,
) {
    match action {
        CanvasContextAction::SetVertexMode(vr, mode) => {
            // Snapshot for undo.
            if let Some(node) = state.scene.get(vr.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: vr.node,
                    path: path.clone(),
                });
            }
            SelectState::set_vertex_mode(&mut state.scene, &vr, mode);
            renderer.mark_dirty();
        }
        CanvasContextAction::DeleteVertex(vr) => {
            // Select the vertex and delete via the existing method.
            state.select_state.selected.clear();
            state.select_state.selected.push(vr);
            // Snapshot for undo.
            if let Some(node) = state.scene.get(vr.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: vr.node,
                    path: path.clone(),
                });
            }
            state
                .select_state
                .delete_selected_vertices(&mut state.scene);
            renderer.mark_dirty();
        }
        CanvasContextAction::ConvertSegment(hit, kind) => {
            // Snapshot for undo.
            if let Some(node) = state.scene.get(hit.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: hit.node,
                    path: path.clone(),
                });
            }
            let changed = match kind {
                vector_tools::SegmentKind::Line => {
                    SelectState::convert_segment_to_line(&mut state.scene, &hit)
                }
                vector_tools::SegmentKind::Quad => {
                    SelectState::convert_segment_to_quad(&mut state.scene, &hit)
                }
                vector_tools::SegmentKind::Cubic => {
                    SelectState::convert_segment_to_cubic(&mut state.scene, &hit)
                }
                vector_tools::SegmentKind::Arc => false, // no conversion to arc yet
            };
            if changed {
                renderer.mark_dirty();
            }
        }
        CanvasContextAction::InsertVertex(hit) => {
            // Snapshot for undo.
            if let Some(node) = state.scene.get(hit.node)
                && let NodeData::Path { ref path, .. } = node.data
            {
                state.history.record_undo(Command::SetPathData {
                    id: hit.node,
                    path: path.clone(),
                });
            }
            if let Some(vr) = SelectState::insert_point_on_edge(&mut state.scene, &hit) {
                state.select_state.selected.clear();
                state.select_state.selected.push(vr);
                renderer.mark_dirty();
            }
        }
    }
}
