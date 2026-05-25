//! Properties panel — fill/stroke/transform editors for the current
//! selection. Shown on the right side of the editor.

use locus_geom::Affine;
use locus_ops::{Command, History};
use locus_render::Renderer;
use locus_scene::{BoolOp, GroupKind, NodeData, Scene};
use locus_tools::SelectState;

use crate::paint_picker;
use crate::structure_panel::{StructureAction, StructureCommands};
use crate::util::{combined_bounds, node_display};

/// Show fill/stroke properties for the current selection.
///
/// `structure_cmds` is the deferred action queue from the structure
/// panel — used to enqueue boolean-group op changes that must run
/// outside the egui pass.
pub(crate) fn show_properties(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    history: &mut History,
    selection: &SelectState,
    renderer: &mut Renderer,
    structure_cmds: &mut StructureCommands,
    stop_focus: &mut Option<(locus_scene::NodeId, usize)>,
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
                scene.set_label(node_ids[0], label);
                renderer.mark_dirty();
            }
        });
        ui.add_space(2.0);
    }

    // World-space bounding box of the current selection. Computed once
    // and shared between the Transform section (W/H/scale fields) and
    // the paint editor (for seeding gradient default geometry).
    let bounds = combined_bounds(scene, &node_ids);

    // ── Transform / Geometry section ──
    {
        use locus_tools::SelectionMode;
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
                                        locus_geom::Point::new(
                                            (node_bounds.min.x + node_bounds.max.x) * 0.5,
                                            (node_bounds.min.y + node_bounds.max.y) * 0.5,
                                        )
                                    } else {
                                        locus_geom::Point::new(old_transform.tx, old_transform.ty)
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
                                        scene.set_transform(node_id, new_local);
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
                                if let Some(node) = scene.get(node_id) {
                                    let mut t = node.transform;
                                    t.tx += dx;
                                    t.ty += dy;
                                    scene.set_transform(node_id, t);
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
                                scene.set_transform(node_id, new_transform);
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
                                    use locus_geom::VertexMode;
                                    use locus_tools::PointKind;
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

                                    // Adjacent-segment type buttons. Lets the
                                    // user make either side of a vertex
                                    // sharp (Line) or curved (Quad/Cubic)
                                    // without having to right-click the
                                    // segment itself. Side rows are only
                                    // drawn when an adjacent segment exists
                                    // (open subpath endpoints get one side).
                                    use locus_tools::SegmentKind;
                                    let render_side_row = |ui: &mut egui::Ui,
                                                           label: &str,
                                                           hit_opt: Option<
                                        locus_tools::EdgeHit,
                                    >|
                                     -> Option<(
                                        locus_tools::EdgeHit,
                                        SegmentKind,
                                    )> {
                                        let hit = hit_opt?;
                                        let current = SelectState::segment_type(scene, &hit)?;
                                        let mut clicked: Option<SegmentKind> = None;
                                        ui.horizontal(|ui| {
                                            ui.label(label);
                                            for (kind, lbl) in [
                                                (SegmentKind::Line, "Line"),
                                                (SegmentKind::Quad, "Quad"),
                                                (SegmentKind::Cubic, "Cubic"),
                                            ] {
                                                let is_active = current == kind;
                                                if ui.selectable_label(is_active, lbl).clicked()
                                                    && !is_active
                                                {
                                                    clicked = Some(kind);
                                                }
                                            }
                                        });
                                        clicked.map(|k| (hit, k))
                                    };

                                    let incoming_hit =
                                        SelectState::incoming_edge_for_vertex(scene, &vref);
                                    let outgoing_hit =
                                        SelectState::outgoing_edge_for_vertex(scene, &vref);

                                    // Resolve conversions outside the closure
                                    // so we can borrow `scene` mutably after.
                                    let in_change = render_side_row(ui, "In", incoming_hit);
                                    let out_change = render_side_row(ui, "Out", outgoing_hit);

                                    for (hit, kind) in [in_change, out_change].into_iter().flatten()
                                    {
                                        if let Some(node) = scene.get(hit.node)
                                            && let NodeData::Path { ref path, .. } = node.data
                                        {
                                            history.record_undo(Command::SetPathData {
                                                id: hit.node,
                                                path: path.clone(),
                                            });
                                        }
                                        let changed = match kind {
                                            SegmentKind::Line => {
                                                SelectState::convert_segment_to_line(scene, &hit)
                                            }
                                            SegmentKind::Quad => {
                                                SelectState::convert_segment_to_quad(scene, &hit)
                                            }
                                            SegmentKind::Cubic => {
                                                SelectState::convert_segment_to_cubic(scene, &hit)
                                            }
                                            SegmentKind::Arc => false,
                                        };
                                        if changed {
                                            renderer.mark_dirty();
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
                                    let new_world = locus_geom::Point::new(vx as f64, vy as f64);
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
                            let mut sel_bounds = locus_geom::Bounds::EMPTY;
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

    // ── Boolean group section (shown when exactly one Boolean group is selected) ──
    if node_ids.len() == 1 {
        let id = node_ids[0];
        let is_boolean = scene
            .get(id)
            .map(|n| {
                matches!(
                    &n.data,
                    NodeData::Group {
                        kind: GroupKind::Boolean { .. },
                        ..
                    }
                )
            })
            .unwrap_or(false);
        if is_boolean {
            egui::CollapsingHeader::new(egui::RichText::new("Boolean").strong())
                .default_open(true)
                .show(ui, |ui| {
                    let current_op = match &scene.get(id).unwrap().data {
                        NodeData::Group {
                            kind: GroupKind::Boolean { op, .. },
                            ..
                        } => *op,
                        _ => BoolOp::Union,
                    };
                    let mut op = current_op;
                    ui.horizontal(|ui| {
                        ui.label("Op");
                        egui::ComboBox::from_id_salt("boolean_op_combo")
                            .selected_text(op_label(op))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut op,
                                    BoolOp::Union,
                                    op_label(BoolOp::Union),
                                );
                                ui.selectable_value(
                                    &mut op,
                                    BoolOp::Difference,
                                    op_label(BoolOp::Difference),
                                );
                                ui.selectable_value(
                                    &mut op,
                                    BoolOp::Intersect,
                                    op_label(BoolOp::Intersect),
                                );
                                ui.selectable_value(
                                    &mut op,
                                    BoolOp::Exclude,
                                    op_label(BoolOp::Exclude),
                                );
                            });
                    });
                    if op != current_op {
                        structure_cmds.push(StructureAction::SetBooleanOp { group: id, op });
                    }
                    if ui.button("Flatten → Path").clicked() {
                        structure_cmds.push(StructureAction::FlattenBooleanGroup { group: id });
                    }
                });
        }
    }

    // Read current style from the first selected node that has one.
    // Both Path nodes and Boolean groups carry a Style; Boolean groups
    // take priority when mixed (single-selection Boolean group exposes
    // its own style editors).
    let reference_style = node_ids.iter().find_map(|&id| {
        let node = scene.get(id)?;
        match &node.data {
            NodeData::Path { style, .. } => Some(style.clone()),
            NodeData::Group {
                kind: GroupKind::Boolean { style, .. },
                ..
            } => Some(style.clone()),
            _ => None,
        }
    });

    let Some(mut style) = reference_style else {
        return;
    };

    let mut changed = false;

    // Reuse the bounds computed at the top of the panel (used to seed
    // gradient default geometry when switching paint type away from solid).
    let bbox = bounds;

    // Gradient edits mutate the scene directly via `scene.set_gradient`
    // and push a `Command::SetGradient` (carrying the previous value)
    // onto this list. We splice these into the SetStyle undo batch
    // below so a frame's worth of paint changes undoes atomically.
    let mut gradient_undos: Vec<Command> = Vec::new();

    // ── Fill section ──
    egui::CollapsingHeader::new(egui::RichText::new("Fill").strong())
        .default_open(true)
        .show(ui, |ui| {
            let mut has_fill = style.fill.is_some();
            if ui.checkbox(&mut has_fill, "Enabled").changed() {
                if has_fill && style.fill.is_none() {
                    style.fill = Some(locus_scene::style::Fill {
                        paint: locus_scene::PaintRef::Solid(locus_geom::Color::BLACK),
                        rule: locus_scene::FillRule::NonZero,
                        opacity: 1.0,
                    });
                } else if !has_fill {
                    style.fill = None;
                }
                changed = true;
            }

            if let Some(ref mut fill) = style.fill {
                if paint_section_editor(
                    ui,
                    scene,
                    &mut fill.paint,
                    bbox,
                    "fill",
                    &mut gradient_undos,
                    stop_focus,
                ) {
                    changed = true;
                }

                ui.horizontal(|ui| {
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
                    style.stroke = Some(locus_scene::style::Stroke {
                        paint: locus_scene::PaintRef::Solid(locus_geom::Color::BLACK),
                        style: locus_scene::StrokeStyle::default(),
                        opacity: 1.0,
                    });
                } else if !has_stroke {
                    style.stroke = None;
                }
                changed = true;
            }

            if let Some(ref mut stroke) = style.stroke {
                if paint_section_editor(
                    ui,
                    scene,
                    &mut stroke.paint,
                    bbox,
                    "stroke",
                    &mut gradient_undos,
                    stop_focus,
                ) {
                    changed = true;
                }

                ui.horizontal(|ui| {
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
                });

                ui.horizontal(|ui| {
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
                });
            }
        });

    // Apply changes to all selected path nodes.
    if changed || !gradient_undos.is_empty() {
        // Snapshot old styles for undo.
        let mut style_undos = Vec::new();
        for &node_id in &node_ids {
            if let Some(node) = scene.get(node_id) {
                match &node.data {
                    NodeData::Path {
                        style: old_style, ..
                    } => {
                        style_undos.push(Command::SetStyle {
                            id: node_id,
                            style: old_style.clone(),
                        });
                    }
                    NodeData::Group {
                        kind:
                            GroupKind::Boolean {
                                op,
                                style: old_style,
                            },
                        ..
                    } => {
                        style_undos.push(Command::SetGroupKind {
                            id: node_id,
                            kind: GroupKind::Boolean {
                                op: *op,
                                style: old_style.clone(),
                            },
                        });
                    }
                    _ => {}
                }
            }
        }

        // Apply new style.
        if changed {
            for &node_id in &node_ids {
                scene.set_style(node_id, style.clone());
            }
        }

        // Build the combined undo. Operations were applied in this order:
        //   1. gradient edits during UI (gradient_undos collected in order)
        //   2. style edits at the end (style_undos collected in iteration order)
        // To undo, we reverse the whole list.
        let mut all_undos = gradient_undos;
        all_undos.extend(style_undos);
        all_undos.reverse();

        if all_undos.len() == 1 {
            history.record_undo(all_undos.into_iter().next().unwrap());
        } else if !all_undos.is_empty() {
            history.record_undo(Command::Batch(all_undos));
        }

        renderer.mark_dirty();
    }
}

// ── Paint type switching + per-kind editors ───────────────────────────────

/// Coarse paint kind, used to drive the type-selector combo. `Other` covers
/// patterns and any future paint variants we don't yet edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaintKind {
    Solid,
    Linear,
    Radial,
    Other,
}

impl PaintKind {
    fn label(self) -> &'static str {
        match self {
            PaintKind::Solid => "Solid",
            PaintKind::Linear => "Linear gradient",
            PaintKind::Radial => "Radial gradient",
            PaintKind::Other => "—",
        }
    }
}

fn paint_kind_of(paint: &locus_scene::PaintRef, scene: &Scene) -> PaintKind {
    use locus_scene::{GradientKind, NodeData, Paint, PaintRef};
    match paint {
        PaintRef::Solid(_) => PaintKind::Solid,
        PaintRef::Ref(id) => match scene.get(*id).map(|n| &n.data) {
            Some(NodeData::Paint(Paint::Gradient(g))) => match g.kind {
                GradientKind::Linear { .. } => PaintKind::Linear,
                GradientKind::Radial { .. } => PaintKind::Radial,
            },
            _ => PaintKind::Other,
        },
    }
}

/// Render the paint editor for a single paint slot (fill OR stroke).
///
/// Lays out a `[Type ▾]` combo on the first line, then either:
///   - a swatch button (Solid), or
///   - a "Shared by N objects [Make unique]" banner + gradient editor
///     stub (Linear / Radial).
///
/// Returns true if the *style* (the local `Style` value owned by the
/// caller) was modified — that signals the caller to apply it via
/// `scene.set_style` and record a SetStyle undo. Gradient mutations to
/// nodes in defs are applied directly to `scene` and pushed onto
/// `gradient_undos` for the caller to splice into the same undo batch.
fn paint_section_editor(
    ui: &mut egui::Ui,
    scene: &mut Scene,
    paint: &mut locus_scene::PaintRef,
    bbox: locus_geom::Bounds,
    id_source: &str,
    gradient_undos: &mut Vec<Command>,
    stop_focus: &mut Option<(locus_scene::NodeId, usize)>,
) -> bool {
    use locus_scene::{NodeData, Paint, PaintRef};

    let mut changed = false;

    // ── Type selector ──
    let current_kind = paint_kind_of(paint, scene);
    let mut next_kind = current_kind;
    ui.horizontal(|ui| {
        ui.label("Type");
        egui::ComboBox::from_id_salt(("paint_kind", id_source))
            .selected_text(current_kind.label())
            .show_ui(ui, |ui| {
                for k in [PaintKind::Solid, PaintKind::Linear, PaintKind::Radial] {
                    ui.selectable_value(&mut next_kind, k, k.label());
                }
            });
    });
    if next_kind != current_kind
        && next_kind != PaintKind::Other
        && let Some(new_paint) = paint_with_kind(scene, paint, next_kind, bbox)
    {
        *paint = new_paint;
        changed = true;
    }

    // ── Body ──
    match paint {
        PaintRef::Solid(c) => {
            ui.horizontal(|ui| {
                ui.label("Color");
                if paint_picker::solid_color_button(ui, c, scene, id_source) {
                    changed = true;
                }
            });
        }
        PaintRef::Ref(grad_id) => {
            let grad_id = *grad_id;

            // Shared-gradient banner.
            let count = scene.gradient_ref_count(grad_id);
            if count > 1 {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("Shared by {count} objects")).weak());
                    if ui.button("Make unique").clicked()
                        && let Some(new_id) = scene.clone_paint_node(grad_id)
                    {
                        *paint = PaintRef::Ref(new_id);
                        changed = true;
                    }
                });
            }

            // Gradient body: clone, let the editor mutate, apply via
            // `set_gradient`. We do this every frame the editor is drawn —
            // changed=false short-circuits the apply pass, so the only
            // overhead when nothing changes is a single Gradient clone.
            if let Some(grad_node) = scene.get(grad_id)
                && let NodeData::Paint(Paint::Gradient(g)) = &grad_node.data
            {
                let mut new_grad = g.clone();
                if crate::gradient_editor::show_gradient_editor(
                    ui,
                    &mut new_grad,
                    scene,
                    id_source,
                    grad_id,
                    stop_focus,
                ) && let Some(old) = scene.set_gradient(grad_id, new_grad)
                {
                    gradient_undos.push(Command::SetGradient {
                        id: grad_id,
                        gradient: old,
                    });
                }
            }
        }
    }

    changed
}

/// Build a new `PaintRef` of `kind`, drawing geometry defaults from `bbox`
/// and a fallback colour from the existing paint. For gradient targets,
/// inserts a fresh `NodeData::Paint(Paint::Gradient(_))` into the defs
/// subtree and returns `PaintRef::Ref(new_id)`.
///
/// The defs node is *not* paired with a `Delete` undo — see comment in
/// the plan: we rely on the SetStyle undo restoring the original
/// `PaintRef`, leaving the gradient orphaned in defs (a known TODO for
/// future GC). This keeps undo correct without depending on stable NodeIds
/// across Insert/InsertSubtree round-trips.
fn paint_with_kind(
    scene: &mut Scene,
    paint: &locus_scene::PaintRef,
    kind: PaintKind,
    bbox: locus_geom::Bounds,
) -> Option<locus_scene::PaintRef> {
    use locus_geom::{Affine, Color, Point};
    use locus_scene::{
        ColorStop, Gradient, GradientKind, InterpolationSpace, Node, NodeData, Paint, PaintRef,
        SpreadMethod,
    };

    let current_color = match paint {
        PaintRef::Solid(c) => *c,
        PaintRef::Ref(id) => match scene.get(*id).map(|n| &n.data) {
            Some(NodeData::Paint(Paint::Gradient(g))) if !g.stops.is_empty() => {
                // Pick the most opaque stop so switch-to-solid lands on a
                // useful colour rather than transparent end-stops.
                let mut best = g.stops[0].color;
                for s in &g.stops {
                    if s.color.a > best.a {
                        best = s.color;
                    }
                }
                best
            }
            _ => Color::WHITE,
        },
    };

    match kind {
        PaintKind::Solid => Some(PaintRef::Solid(current_color)),
        PaintKind::Linear => {
            let (start, end) = if bbox.is_empty() {
                (Point::new(0.0, 0.0), Point::new(100.0, 0.0))
            } else {
                (
                    Point::new(bbox.min.x, bbox.min.y),
                    Point::new(bbox.max.x, bbox.min.y),
                )
            };
            let g = Gradient {
                kind: GradientKind::Linear { start, end },
                stops: vec![
                    ColorStop {
                        offset: 0.0,
                        color: Color {
                            a: 0.0,
                            ..current_color
                        },
                    },
                    ColorStop {
                        offset: 1.0,
                        color: current_color,
                    },
                ],
                interpolation: InterpolationSpace::default(),
                transform: Affine::IDENTITY,
                spread: SpreadMethod::default(),
            };
            let defs = scene.defs();
            let node = Node {
                label: "linearGradient".into(),
                transform: Affine::IDENTITY,
                data: NodeData::Paint(Paint::Gradient(g)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            scene.insert(defs, node).map(PaintRef::Ref)
        }
        PaintKind::Radial => {
            let (center, radius) = if bbox.is_empty() {
                (Point::new(0.0, 0.0), 50.0)
            } else {
                let cx = (bbox.min.x + bbox.max.x) * 0.5;
                let cy = (bbox.min.y + bbox.max.y) * 0.5;
                let r = (bbox.width().min(bbox.height())) * 0.5;
                (Point::new(cx, cy), r.max(1.0))
            };
            let g = Gradient {
                kind: GradientKind::Radial {
                    center,
                    radius,
                    focal: center,
                    focal_radius: 0.0,
                },
                stops: vec![
                    ColorStop {
                        offset: 0.0,
                        color: current_color,
                    },
                    ColorStop {
                        offset: 1.0,
                        color: Color {
                            a: 0.0,
                            ..current_color
                        },
                    },
                ],
                interpolation: InterpolationSpace::default(),
                transform: Affine::IDENTITY,
                spread: SpreadMethod::default(),
            };
            let defs = scene.defs();
            let node = Node {
                label: "radialGradient".into(),
                transform: Affine::IDENTITY,
                data: NodeData::Paint(Paint::Gradient(g)),
                children: Vec::new(),
                visible: true,
                locked: false,
            };
            scene.insert(defs, node).map(PaintRef::Ref)
        }
        PaintKind::Other => None,
    }
}

fn op_label(op: BoolOp) -> &'static str {
    match op {
        BoolOp::Union => "Union",
        BoolOp::Difference => "Difference",
        BoolOp::Intersect => "Intersect",
        BoolOp::Exclude => "Exclude (XOR)",
    }
}
