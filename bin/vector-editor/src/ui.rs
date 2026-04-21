//! Top-level UI layout — menu bar, tools panel, inspector panel (properties
//! + structure). The canvas area is NOT part of any egui Ui: we use
//!   `begin_pass`/`end_pass` with `Panel::show` on the Context directly,
//!   so `is_pointer_over_egui()` returns false over the canvas.

use vector_ops::{Command, History};
use vector_render::Renderer;
use vector_tools::{PenAction, SelectState, TextAction, ToolType};

use crate::editor_state::EditorState;
use crate::hud::format_count;
use crate::properties_panel::show_properties;
use crate::structure_panel::{
    ReorderCommand, StructureAction, StructureCommands, flatten_tree, render_virtual_row,
};

/// UI layout — uses begin_pass/end_pass with Panel::show on the Context directly,
/// so the canvas area is NOT part of any egui Ui. This lets
/// `is_pointer_over_egui()` return false for the canvas.
///
/// Returns (structure_commands, scene_dirty) from the structure panel.
#[expect(deprecated)] // Panel::show is deprecated in 0.34 but needed for top-level panels
pub(crate) fn run_ui(
    ctx: &egui::Context,
    state: &mut EditorState,
    renderer: &mut Renderer,
) -> (StructureCommands, bool) {
    let scene = &mut state.scene;
    let history = &mut state.history;
    let active_tool = &mut state.active_tool;
    let selection = &mut state.select_state;
    let pen_state = &mut state.pen_state;
    let text_tool = &mut state.text_tool;
    let pending_zoom_to_fit = &mut state.pending_zoom_to_fit;
    let snap = &mut state.snap;
    let checker_fixed_size = &mut state.checker_fixed_size;
    let show_perf_hud = &mut state.show_perf_hud;
    let perf = &state.perf;
    let structure_collapse = &mut state.structure_collapse;
    let mut dump_requested = false;
    let mut reorder_cmd: ReorderCommand = None;
    let mut structure_cmds: StructureCommands = Vec::new();
    let mut scene_dirty = false;
    let mut open_requested = false;
    let mut save_requested = false;
    let mut trace_requested = false;
    let mut undo_requested = false;
    let mut redo_requested = false;

    let menu_resp = egui::Panel::top("menu_bar").show(ctx, |ui| {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open SVG...").clicked() {
                    open_requested = true;
                    ui.close();
                }
                if ui.button("Save SVG...").clicked() {
                    save_requested = true;
                    ui.close();
                }
                ui.separator();
                if ui.button("Trace raster image...").clicked() {
                    trace_requested = true;
                    ui.close();
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui
                    .add_enabled(history.can_undo(), egui::Button::new("Undo  Ctrl+Z"))
                    .clicked()
                {
                    undo_requested = true;
                    ui.close();
                }
                if ui
                    .add_enabled(history.can_redo(), egui::Button::new("Redo  Ctrl+Shift+Z"))
                    .clicked()
                {
                    redo_requested = true;
                    ui.close();
                }
            });
            ui.menu_button("Path", |ui| {
                // Boolean group operations — wrap ≥2 selected nodes in a
                // new non-destructive boolean group. Originals are
                // preserved as the group's children; editing them
                // recomputes the result.
                let sel_count = selection.selected_nodes.len();
                let enabled = sel_count >= 2;
                if ui
                    .add_enabled(enabled, egui::Button::new("Union  Ctrl++"))
                    .on_hover_text("Combine selected paths into one Boolean(Union) group")
                    .clicked()
                {
                    structure_cmds.push(StructureAction::MakeBooleanGroup {
                        nodes: selection.selected_nodes.clone(),
                        op: vector_scene::BoolOp::Union,
                    });
                    ui.close();
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Difference  Ctrl+-"))
                    .on_hover_text(
                        "Subtract upper paths from the lowest selected path (Boolean group)",
                    )
                    .clicked()
                {
                    structure_cmds.push(StructureAction::MakeBooleanGroup {
                        nodes: selection.selected_nodes.clone(),
                        op: vector_scene::BoolOp::Difference,
                    });
                    ui.close();
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Intersect  Ctrl+*"))
                    .on_hover_text("Keep the overlapping region of the selected paths")
                    .clicked()
                {
                    structure_cmds.push(StructureAction::MakeBooleanGroup {
                        nodes: selection.selected_nodes.clone(),
                        op: vector_scene::BoolOp::Intersect,
                    });
                    ui.close();
                }
                if ui
                    .add_enabled(enabled, egui::Button::new("Exclude (XOR)  Ctrl+^"))
                    .on_hover_text("Keep the symmetric difference of the selected paths")
                    .clicked()
                {
                    structure_cmds.push(StructureAction::MakeBooleanGroup {
                        nodes: selection.selected_nodes.clone(),
                        op: vector_scene::BoolOp::Exclude,
                    });
                    ui.close();
                }

                ui.separator();

                // "Flatten" — bake a selected Boolean group into a single
                // Path node. Destroys the non-destructive structure.
                let flatten_target = selection.selected_nodes.iter().copied().find(|&id| {
                    matches!(
                        scene.get(id).map(|n| &n.data),
                        Some(vector_scene::NodeData::Group {
                            kind: vector_scene::GroupKind::Boolean { .. },
                            ..
                        })
                    )
                });
                if ui
                    .add_enabled(
                        flatten_target.is_some(),
                        egui::Button::new("Flatten Boolean → Path"),
                    )
                    .on_hover_text(
                        "Replace the selected Boolean group with its baked result as a single path",
                    )
                    .clicked()
                {
                    if let Some(group) = flatten_target {
                        structure_cmds.push(StructureAction::FlattenBooleanGroup { group });
                    }
                    ui.close();
                }
            });
            ui.menu_button("View", |ui| {
                ui.checkbox(&mut snap.grid_enabled, "Snap to grid");
                ui.horizontal(|ui| {
                    ui.label("Grid size:");
                    ui.add(
                        egui::DragValue::new(&mut snap.grid_size)
                            .range(0.1..=100.0)
                            .speed(0.1)
                            .suffix(" px"),
                    );
                });
                ui.separator();
                ui.checkbox(checker_fixed_size, "Fixed-size checkerboard");
                ui.separator();
                ui.checkbox(show_perf_hud, "Show FPS / RSS");
            });
            ui.menu_button("Debug", |ui| {
                if ui.button("Dump layout").clicked() {
                    dump_requested = true;
                    ui.close();
                }
            });

            // Performance HUD, right-aligned. Laying out right-to-left means
            // items are added from the right edge inward — so the last thing
            // added appears leftmost of the group.
            if *show_perf_hud {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let fps = perf.fps();
                    let ms = perf.frame_ms();
                    let rss = perf.rss_display();
                    let stats = renderer.scene_stats();
                    // right_to_left: items are added right-edge first, so
                    // to read RSS | FPS | geometry left→right we push in
                    // reverse here.
                    ui.label(egui::RichText::new(rss).monospace().weak())
                        .on_hover_text("Resident set size (process physical memory)");
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!("{fps:5.1} fps  {ms:4.1} ms"))
                            .monospace()
                            .weak(),
                    )
                    .on_hover_text("Frame rate (vsync-capped)");
                    ui.separator();
                    ui.label(
                        egui::RichText::new(format!(
                            "{} paths  {} vtx  {} tri",
                            format_count(stats.paths),
                            format_count(stats.vertices),
                            format_count(stats.triangles),
                        ))
                        .monospace()
                        .weak(),
                    )
                    .on_hover_text(
                        "Post-tessellation geometry in the scene \
                         (excluding grid and handle overlays)",
                    );
                });
            }
        });
    });

    let tools_resp = egui::Panel::left("tools_panel")
        .default_size(120.0)
        .resizable(false)
        .show(ctx, |ui| {
            ui.vertical(|ui| {
                for &tool in ToolType::ALL {
                    let label = format!("{} {}", tool.icon(), tool.name());
                    if ui.selectable_label(*active_tool == tool, label).clicked() {
                        // Finish pen path if switching away from pen tool.
                        if *active_tool == ToolType::Pen
                            && tool != ToolType::Pen
                            && pen_state.is_building()
                        {
                            let action = pen_state.finish(scene, false);
                            match action {
                                PenAction::Finished(node_id) => {
                                    history.record_undo(Command::Delete { id: node_id });
                                    selection.selected_nodes.clear();
                                    selection.selected_nodes.push(node_id);
                                    renderer.mark_dirty();
                                }
                                PenAction::Cancelled => {
                                    selection.selected_nodes.clear();
                                    renderer.mark_dirty();
                                }
                                _ => {}
                            }
                        }
                        // Commit text edit if switching away from text tool.
                        if *active_tool == ToolType::Text
                            && tool != ToolType::Text
                            && text_tool.is_editing()
                        {
                            let action = text_tool.commit(scene);
                            match action {
                                TextAction::Committed(node_id, original) => {
                                    history.record_undo(Command::SetTextData {
                                        id: node_id,
                                        text: original,
                                    });
                                    selection.selected_nodes.clear();
                                    selection.selected_nodes.push(node_id);
                                    renderer.mark_dirty();
                                }
                                TextAction::Cancelled => {
                                    selection.selected_nodes.clear();
                                    renderer.mark_dirty();
                                }
                                _ => {}
                            }
                        }
                        *active_tool = tool;
                    }
                }
            });
        });

    let mut properties_rect = egui::Rect::NOTHING;
    let mut structure_rect = egui::Rect::NOTHING;

    let inspector_resp = egui::Panel::right("inspector_panel")
        .default_size(220.0)
        .show(ctx, |ui| {
            let half = ui.available_height() * 0.5;

            // Properties — top half
            let props_resp = egui::Panel::top("properties_section")
                .default_size(half)
                .min_size(half)
                .show_inside(ui, |ui| {
                    profiling::scope!("properties_panel");
                    ui.heading("Properties");
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("properties_scroll")
                        .show(ui, |ui| {
                            show_properties(
                                ui,
                                scene,
                                history,
                                selection,
                                renderer,
                                &mut structure_cmds,
                            );
                        });
                });
            properties_rect = props_resp.response.rect;

            // Structure — bottom half (scene graph tree)
            let structure_resp = egui::CentralPanel::default().show_inside(ui, |ui| {
                profiling::scope!("structure_panel");
                ui.horizontal(|ui| {
                    ui.heading("Structure");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .small_button(egui_phosphor::regular::FOLDER_PLUS)
                            .on_hover_text("New Group")
                            .clicked()
                        {
                            structure_cmds.push(StructureAction::NewGroup {
                                parent: scene.root(),
                            });
                        }
                        // "Group Selection" button — enabled when ≥1 node selected
                        let has_selection = !selection.selected_nodes.is_empty();
                        if ui
                            .add_enabled(
                                has_selection,
                                egui::Button::new(egui_phosphor::regular::SELECTION_ALL).small(),
                            )
                            .on_hover_text("Group Selection")
                            .clicked()
                        {
                            structure_cmds.push(StructureAction::GroupSelection {
                                nodes: selection.selected_nodes.clone(),
                            });
                        }
                    });
                });
                ui.separator();

                // Virtualized tree: flatten the visible tree O(n), then use
                // `ScrollArea::show_rows` so egui only builds widgets for
                // rows inside the viewport. This makes per-frame cost
                // proportional to visible rows, not total scene size.
                let flat = {
                    profiling::scope!("flatten_tree");
                    flatten_tree(scene, structure_collapse)
                };

                let row_height = ui.spacing().interact_size.y;
                egui::ScrollArea::vertical()
                    .id_salt("structure_scroll")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, flat.len(), |ui, row_range| {
                        profiling::scope!("virtual_rows", &format!("{}", row_range.len()));
                        for i in row_range {
                            render_virtual_row(
                                ui,
                                &flat[i],
                                row_height,
                                scene,
                                structure_collapse,
                                selection,
                                &mut reorder_cmd,
                                &mut structure_cmds,
                                &mut scene_dirty,
                            );
                        }
                    });
            });
            structure_rect = structure_resp.response.rect;
        });

    if dump_requested {
        fn fmt_rect(name: &str, r: egui::Rect) -> String {
            format!(
                "  {name:20} pos=({:.0}, {:.0})  size=({:.0} x {:.0})",
                r.min.x,
                r.min.y,
                r.width(),
                r.height()
            )
        }
        #[expect(deprecated)]
        let available = ctx.available_rect();
        log::info!("=== egui layout dump ===");
        log::info!("{}", fmt_rect("canvas (available)", available));
        log::info!("{}", fmt_rect("menu_bar", menu_resp.response.rect));
        log::info!("{}", fmt_rect("tools_panel", tools_resp.response.rect));
        log::info!(
            "{}",
            fmt_rect("inspector_panel", inspector_resp.response.rect)
        );
        log::info!("{}", fmt_rect("  properties", properties_rect));
        log::info!("{}", fmt_rect("  structure", structure_rect));
        log::info!("========================");
    }

    // ── Undo/redo from menu ──

    if undo_requested && !pen_state.is_building() && history.can_undo() {
        history.undo(scene);
        selection.selected_nodes.clear();
        selection.selected.clear();
        selection.hovered = None;
        renderer.mark_dirty();
    }

    if redo_requested && !pen_state.is_building() && history.can_redo() {
        history.redo(scene);
        selection.selected_nodes.clear();
        selection.selected.clear();
        selection.hovered = None;
        renderer.mark_dirty();
    }

    // ── File dialogs (blocking — runs after egui pass) ──

    if open_requested {
        // Finish any in-progress pen path before loading.
        if pen_state.is_building() {
            let action = pen_state.finish(scene, false);
            match action {
                PenAction::Finished(node_id) => {
                    selection.selected_nodes.clear();
                    selection.selected_nodes.push(node_id);
                    renderer.mark_dirty();
                }
                PenAction::Cancelled => {
                    selection.selected_nodes.clear();
                    renderer.mark_dirty();
                }
                _ => {}
            }
        }

        if let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG files", &["svg"])
            .pick_file()
        {
            match std::fs::read(&path) {
                Ok(data) => match vector_svg::import_svg(&data) {
                    Ok(new_scene) => {
                        *scene = new_scene;
                        *history = History::new(); // clear undo/redo for old scene
                        *selection = SelectState::default();
                        *pending_zoom_to_fit = true;
                        renderer.mark_dirty();
                        log::info!("Opened SVG: {}", path.display());
                    }
                    Err(e) => log::error!("Failed to import SVG: {e}"),
                },
                Err(e) => log::error!("Failed to read file: {e}"),
            }
        }
    }

    if save_requested
        && let Some(path) = rfd::FileDialog::new()
            .add_filter("SVG files", &["svg"])
            .set_file_name("untitled.svg")
            .save_file()
    {
        let svg = vector_svg::export_svg(scene);
        match std::fs::write(&path, &svg) {
            Ok(()) => log::info!("Saved SVG: {}", path.display()),
            Err(e) => log::error!("Failed to save file: {e}"),
        }
    }

    if trace_requested
        && let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Raster images",
                &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif"],
            )
            .pick_file()
    {
        match std::fs::read(&path) {
            Ok(data) => {
                match vector_trace::trace_image_bytes(&data, vector_trace::TracePreset::default()) {
                    Ok(new_scene) => {
                        *scene = new_scene;
                        *history = History::new();
                        *selection = SelectState::default();
                        *pending_zoom_to_fit = true;
                        renderer.mark_dirty();
                        log::info!("Traced raster: {}", path.display());
                    }
                    Err(e) => log::error!("Failed to trace image: {e}"),
                }
            }
            Err(e) => log::error!("Failed to read file: {e}"),
        }
    }

    // Convert drag-and-drop reorder to a structure action.
    if let Some((node_id, new_parent, index)) = reorder_cmd {
        structure_cmds.push(StructureAction::Reparent {
            id: node_id,
            new_parent,
            index,
        });
    }

    (structure_cmds, scene_dirty)
}
