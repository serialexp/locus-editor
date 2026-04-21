//! Properties panel — fill/stroke/transform editors for the current
//! selection. Shown on the right side of the editor.

use vector_geom::Affine;
use vector_ops::{Command, History};
use vector_render::Renderer;
use vector_scene::{NodeData, Scene};
use vector_tools::SelectState;

use crate::util::{color_to_egui, combined_bounds, egui_to_color, node_display};

/// Show fill/stroke properties for the current selection.
pub(crate) fn show_properties(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    history: &mut History,
    selection: &SelectState,
    renderer: &mut Renderer,
) {
    if selection.selected_nodes.is_empty() {
        ui.label("No selection");
        return;
    }

    // Collect the node IDs so we don't borrow selection during scene mutation.
    let node_ids: Vec<_> = selection.selected_nodes.clone();

    if node_ids.len() > 1 {
        ui.small(format!("{} objects selected", node_ids.len()));
        ui.add_space(2.0);
    }

    // ── Name field (single selection) ──
    if node_ids.len() == 1
        && let Some(node) = scene.get(node_ids[0])
    {
        let mut label = node.label.clone();
        let (icon, default_name) = node_display(node);
        let hint = default_name.clone();
        ui.horizontal(|ui| {
            ui.label(format!("{icon} Name"));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut label)
                    .hint_text(hint)
                    .desired_width(ui.available_width()),
            );
            if resp.changed() {
                if let Some(n) = scene.get_mut(node_ids[0]) {
                    n.label = label;
                }
                renderer.mark_dirty();
            }
        });
        ui.add_space(2.0);
    }

    // ── Transform / Geometry section ──
    {
        use vector_tools::SelectionMode;
        let header = match selection.mode {
            SelectionMode::Object => "Transform",
            SelectionMode::Node if selection.selected.len() == 1 => "Vertex",
            SelectionMode::Node => "Selection",
        };

        egui::CollapsingHeader::new(egui::RichText::new(header).strong())
            .default_open(true)
            .show(ui, |ui| {
                match selection.mode {
                    SelectionMode::Object => {
                        let bounds = combined_bounds(scene, &node_ids);
                        // Position from the first selected node's transform.
                        let (orig_tx, orig_ty) = scene
                            .get(node_ids[0])
                            .map(|n| (n.transform.tx, n.transform.ty))
                            .unwrap_or((0.0, 0.0));
                        let mut new_tx = orig_tx as f32;
                        let mut new_ty = orig_ty as f32;
                        let orig_w = bounds.width() as f32;
                        let orig_h = bounds.height() as f32;
                        let mut new_w = orig_w;
                        let mut new_h = orig_h;

                        egui::Grid::new("transform_grid")
                            .num_columns(4)
                            .spacing([4.0, 4.0])
                            .show(ui, |ui| {
                                ui.label("X");
                                ui.add(
                                    egui::DragValue::new(&mut new_tx)
                                        .speed(0.5)
                                        .fixed_decimals(1),
                                );
                                ui.label("Y");
                                ui.add(
                                    egui::DragValue::new(&mut new_ty)
                                        .speed(0.5)
                                        .fixed_decimals(1),
                                );
                                ui.end_row();

                                if !bounds.is_empty() {
                                    ui.label("W");
                                    ui.add(
                                        egui::DragValue::new(&mut new_w)
                                            .speed(0.5)
                                            .fixed_decimals(1)
                                            .range(0.001..=f32::INFINITY),
                                    );
                                    ui.label("H");
                                    ui.add(
                                        egui::DragValue::new(&mut new_h)
                                            .speed(0.5)
                                            .fixed_decimals(1)
                                            .range(0.001..=f32::INFINITY),
                                    );
                                    ui.end_row();
                                }
                            });

                        // Rotation field (single selection only — multi-select
                        // rotation from properties is ambiguous).
                        if node_ids.len() == 1 {
                            let orig_deg = scene
                                .get(node_ids[0])
                                .map(|n| n.transform.rotation_deg())
                                .unwrap_or(0.0);
                            let mut new_deg = orig_deg as f32;

                            ui.horizontal(|ui| {
                                ui.label(format!(
                                    "{} Rotation",
                                    egui_phosphor::regular::ARROW_CLOCKWISE
                                ));
                                ui.add(
                                    egui::DragValue::new(&mut new_deg)
                                        .speed(0.5)
                                        .fixed_decimals(1)
                                        .suffix("°"),
                                );
                            });

                            if (new_deg - orig_deg as f32).abs() > 1e-4 {
                                let node_id = node_ids[0];
                                if let Some(node) = scene.get(node_id) {
                                    let old_transform = node.transform;
                                    // Decompose → recompose with new angle.
                                    // We rotate around the node's bounding box center.
                                    let world = scene.world_transform(node_id);
                                    let node_bounds = node.data.visual_bounds(world);
                                    let center = if !node_bounds.is_empty() {
                                        vector_geom::Point::new(
                                            (node_bounds.min.x + node_bounds.max.x) * 0.5,
                                            (node_bounds.min.y + node_bounds.max.y) * 0.5,
                                        )
                                    } else {
                                        vector_geom::Point::new(old_transform.tx, old_transform.ty)
                                    };

                                    let delta_rad = (new_deg as f64 - orig_deg).to_radians();
                                    let rot = Affine::rotate_around(delta_rad, center);
                                    let parent_world = scene.parent_world_transform(node_id);
                                    let new_world = rot.then(parent_world.then(old_transform));
                                    if let Some(inv_parent) = parent_world.inverse() {
                                        let new_local = inv_parent.then(new_world);
                                        history.record_undo(Command::SetTransform {
                                            id: node_id,
                                            transform: old_transform,
                                        });
                                        if let Some(n) = scene.get_mut(node_id) {
                                            n.transform = new_local;
                                        }
                                        renderer.mark_dirty();
                                    }
                                }
                            }
                        }

                        // Apply position changes.
                        let pos_changed = (new_tx - orig_tx as f32).abs() > 1e-6
                            || (new_ty - orig_ty as f32).abs() > 1e-6;
                        if pos_changed {
                            let dx = new_tx as f64 - orig_tx;
                            let dy = new_ty as f64 - orig_ty;
                            let mut undo_cmds = Vec::new();
                            for &node_id in &node_ids {
                                if let Some(node) = scene.get(node_id) {
                                    undo_cmds.push(Command::SetTransform {
                                        id: node_id,
                                        transform: node.transform,
                                    });
                                }
                            }
                            for &node_id in &node_ids {
                                if let Some(node) = scene.get_mut(node_id) {
                                    node.transform.tx += dx;
                                    node.transform.ty += dy;
                                }
                            }
                            if undo_cmds.len() == 1 {
                                history.record_undo(undo_cmds.into_iter().next().unwrap());
                            } else if !undo_cmds.is_empty() {
                                history.record_undo(Command::Batch(undo_cmds));
                            }
                            renderer.mark_dirty();
                        }

                        // Apply size changes (scale around object's bounds center).
                        let size_changed = !bounds.is_empty()
                            && ((new_w - orig_w).abs() > 1e-6 || (new_h - orig_h).abs() > 1e-6);
                        if size_changed && node_ids.len() == 1 {
                            let sx = if orig_w.abs() > 1e-6 {
                                new_w as f64 / orig_w as f64
                            } else {
                                1.0
                            };
                            let sy = if orig_h.abs() > 1e-6 {
                                new_h as f64 / orig_h as f64
                            } else {
                                1.0
                            };
                            let node_id = node_ids[0];
                            if let Some(node) = scene.get(node_id) {
                                let old_transform = node.transform;
                                // Scale the transform: multiply a/b by sx, c/d by sy.
                                // This scales the node's local coordinate system.
                                let mut new_transform = old_transform;
                                new_transform.a *= sx;
                                new_transform.b *= sx;
                                new_transform.c *= sy;
                                new_transform.d *= sy;
                                history.record_undo(Command::SetTransform {
                                    id: node_id,
                                    transform: old_transform,
                                });
                                if let Some(node) = scene.get_mut(node_id) {
                                    node.transform = new_transform;
                                }
                                renderer.mark_dirty();
                            }
                        }
                    }
                    SelectionMode::Node => {
                        if selection.selected.len() == 1 {
                            // Single vertex — show its world-space position.
                            let vref = selection.selected[0];
                            if let Some(local_pos) = vref.get_position(scene) {
                                let world = scene.world_transform(vref.node);
                                let wp = world.apply(local_pos);

                                // Show vertex mode toggle buttons for the
                                // anchor this point belongs to.
                                if let Some(node) = scene.get(vref.node)
                                    && let NodeData::Path { ref path, .. } = node.data
                                    && let Some(subpath) = path.subpaths.get(vref.subpath)
                                {
                                    use vector_geom::VertexMode;
                                    use vector_tools::PointKind;
                                    let mode_idx = match vref.kind {
                                        PointKind::SubpathStart => 0,
                                        PointKind::Endpoint => vref.segment + 1,
                                        PointKind::CubicCtrl1 | PointKind::QuadCtrl => vref.segment,
                                        PointKind::CubicCtrl2 => vref.segment + 1,
                                    };
                                    if let Some(current_mode) =
                                        subpath.vertex_modes.get(mode_idx).copied()
                                    {
                                        let mut mode_changed = None;
                                        ui.horizontal(|ui| {
                                            for (mode, label) in [
                                                (VertexMode::Corner, "Corner"),
                                                (VertexMode::Smooth, "Smooth"),
                                                (VertexMode::Symmetric, "Symmetric"),
                                            ] {
                                                let is_active = current_mode == mode;
                                                if ui.selectable_label(is_active, label).clicked()
                                                    && !is_active
                                                {
                                                    mode_changed = Some(mode);
                                                }
                                            }
                                        });
                                        if let Some(new_mode) = mode_changed {
                                            // Snapshot path for undo before
                                            // changing the mode.
                                            if let Some(node) = scene.get(vref.node)
                                                && let NodeData::Path { ref path, .. } = node.data
                                            {
                                                history.record_undo(Command::SetPathData {
                                                    id: vref.node,
                                                    path: path.clone(),
                                                });
                                            }
                                            if SelectState::set_vertex_mode(scene, &vref, new_mode)
                                                .is_some()
                                            {
                                                renderer.mark_dirty();
                                            }
                                        }
                                    }
                                }

                                let mut vx = wp.x as f32;
                                let mut vy = wp.y as f32;

                                egui::Grid::new("vertex_grid")
                                    .num_columns(4)
                                    .spacing([4.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.label("X");
                                        ui.add(
                                            egui::DragValue::new(&mut vx)
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.label("Y");
                                        ui.add(
                                            egui::DragValue::new(&mut vy)
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.end_row();
                                    });

                                // Apply vertex position change.
                                let vtx_changed = (vx - wp.x as f32).abs() > 1e-6
                                    || (vy - wp.y as f32).abs() > 1e-6;
                                if vtx_changed {
                                    // Convert new world position to local delta.
                                    let new_world = vector_geom::Point::new(vx as f64, vy as f64);
                                    if let Some(inv_world) = world.inverse() {
                                        let new_local = inv_world.apply(new_world);
                                        let dx = new_local.x - local_pos.x;
                                        let dy = new_local.y - local_pos.y;

                                        // Snapshot path for undo.
                                        if let Some(node) = scene.get(vref.node)
                                            && let NodeData::Path { ref path, .. } = node.data
                                        {
                                            history.record_undo(Command::SetPathData {
                                                id: vref.node,
                                                path: path.clone(),
                                            });
                                        }

                                        vref.translate(scene, dx, dy);
                                        renderer.mark_dirty();
                                    }
                                }
                            }
                        } else if selection.selected.len() > 1 {
                            // Multiple vertices — show the bounding box of the selection (read-only).
                            let mut sel_bounds = vector_geom::Bounds::EMPTY;
                            for vref in &selection.selected {
                                if let Some(local_pos) = vref.get_position(scene) {
                                    let world = scene.world_transform(vref.node);
                                    let wp = world.apply(local_pos);
                                    sel_bounds = sel_bounds.include_point(wp);
                                }
                            }
                            if !sel_bounds.is_empty() {
                                ui.small(format!("{} vertices selected", selection.selected.len()));
                                egui::Grid::new("vertex_sel_grid")
                                    .num_columns(4)
                                    .spacing([4.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.label("X");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.min.x as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.label("Y");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.min.y as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.end_row();

                                        ui.label("W");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.width() as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.label("H");
                                        ui.add(
                                            egui::DragValue::new(&mut (sel_bounds.height() as f32))
                                                .speed(0.5)
                                                .fixed_decimals(1),
                                        );
                                        ui.end_row();
                                    });
                            }
                        } else {
                            ui.small("No vertices selected");
                        }
                    }
                }
            });
    }

    // Read current style from the first selected path node.
    let reference_style = node_ids.iter().find_map(|&id| {
        let node = scene.get(id)?;
        match &node.data {
            vector_scene::NodeData::Path { style, .. } => Some(style.clone()),
            _ => None,
        }
    });

    let Some(mut style) = reference_style else {
        return;
    };

    let mut changed = false;

    // ── Fill section ──
    egui::CollapsingHeader::new(egui::RichText::new("Fill").strong())
        .default_open(true)
        .show(ui, |ui| {
            let mut has_fill = style.fill.is_some();
            if ui.checkbox(&mut has_fill, "Enabled").changed() {
                if has_fill && style.fill.is_none() {
                    style.fill = Some(vector_scene::style::Fill {
                        paint: vector_scene::PaintRef::Solid(vector_geom::Color::BLACK),
                        rule: vector_scene::FillRule::NonZero,
                        opacity: 1.0,
                    });
                } else if !has_fill {
                    style.fill = None;
                }
                changed = true;
            }

            if let Some(ref mut fill) = style.fill {
                egui::Grid::new("fill_grid")
                    .num_columns(2)
                    .spacing([4.0, 4.0])
                    .show(ui, |ui| {
                        if let vector_scene::PaintRef::Solid(ref mut color) = fill.paint {
                            ui.label("Color");
                            let mut egui_color = color_to_egui(*color);
                            if ui.color_edit_button_srgba(&mut egui_color).changed() {
                                *color = egui_to_color(egui_color);
                                changed = true;
                            }
                            ui.end_row();
                        }

                        ui.label("Opacity");
                        if ui
                            .add(
                                egui::DragValue::new(&mut fill.opacity)
                                    .range(0.0..=1.0)
                                    .speed(0.01)
                                    .fixed_decimals(2),
                            )
                            .changed()
                        {
                            changed = true;
                        }
                        ui.end_row();
                    });
            }
        });

    // ── Stroke section ──
    egui::CollapsingHeader::new(egui::RichText::new("Stroke").strong())
        .default_open(true)
        .show(ui, |ui| {
            let mut has_stroke = style.stroke.is_some();
            if ui.checkbox(&mut has_stroke, "Enabled").changed() {
                if has_stroke && style.stroke.is_none() {
                    style.stroke = Some(vector_scene::style::Stroke {
                        paint: vector_scene::PaintRef::Solid(vector_geom::Color::BLACK),
                        style: vector_scene::StrokeStyle::default(),
                        opacity: 1.0,
                    });
                } else if !has_stroke {
                    style.stroke = None;
                }
                changed = true;
            }

            if let Some(ref mut stroke) = style.stroke {
                egui::Grid::new("stroke_grid")
                    .num_columns(2)
                    .spacing([4.0, 4.0])
                    .show(ui, |ui| {
                        if let vector_scene::PaintRef::Solid(ref mut color) = stroke.paint {
                            ui.label("Color");
                            let mut egui_color = color_to_egui(*color);
                            if ui.color_edit_button_srgba(&mut egui_color).changed() {
                                *color = egui_to_color(egui_color);
                                changed = true;
                            }
                            ui.end_row();
                        }

                        ui.label("Width");
                        let mut width = stroke.style.width as f32;
                        if ui
                            .add(
                                egui::DragValue::new(&mut width)
                                    .range(0.0..=50.0)
                                    .speed(0.5)
                                    .fixed_decimals(1),
                            )
                            .changed()
                        {
                            stroke.style.width = width as f64;
                            changed = true;
                        }
                        ui.end_row();

                        ui.label("Opacity");
                        if ui
                            .add(
                                egui::DragValue::new(&mut stroke.opacity)
                                    .range(0.0..=1.0)
                                    .speed(0.01)
                                    .fixed_decimals(2),
                            )
                            .changed()
                        {
                            changed = true;
                        }
                        ui.end_row();
                    });
            }
        });

    // Apply changes to all selected path nodes.
    if changed {
        // Snapshot old styles for undo.
        let mut undo_cmds = Vec::new();
        for &node_id in &node_ids {
            if let Some(node) = scene.get(node_id)
                && let vector_scene::NodeData::Path {
                    style: ref old_style,
                    ..
                } = node.data
            {
                undo_cmds.push(Command::SetStyle {
                    id: node_id,
                    style: old_style.clone(),
                });
            }
        }

        // Apply new style.
        for &node_id in &node_ids {
            if let Some(node) = scene.get_mut(node_id)
                && let vector_scene::NodeData::Path {
                    style: ref mut node_style,
                    ..
                } = node.data
            {
                *node_style = style.clone();
            }
        }

        // Record undo.
        if undo_cmds.len() == 1 {
            history.record_undo(undo_cmds.into_iter().next().unwrap());
        } else if !undo_cmds.is_empty() {
            history.record_undo(Command::Batch(undo_cmds));
        }

        renderer.mark_dirty();
    }
}
