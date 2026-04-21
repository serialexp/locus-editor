//! Structure panel — the virtualized tree view on the left side of the
//! editor. Builds a flat list of visible rows from the scene graph each
//! frame (respecting collapse state), then renders only the rows that fall
//! within the scroll viewport via `ScrollArea::show_rows`.

use std::collections::HashMap;

use vector_ops::Command;
use vector_scene::{BoolOp, GroupKind, NodeData, NodeId, Scene, Style};
use vector_tools::SelectState;

use crate::editor_state::EditorState;
use crate::util::node_display;

/// A pending reorder: (node to move, new parent, index within new parent).
pub(crate) type ReorderCommand = Option<(NodeId, NodeId, usize)>;

/// Deferred structural commands from the structure panel context menu.
/// Applied after the egui pass to avoid mutating the tree while iterating it.
pub(crate) type StructureCommands = Vec<StructureAction>;

/// An action from the structure panel that modifies the scene tree.
pub(crate) enum StructureAction {
    /// Reorder/reparent a node (from drag-and-drop).
    Reparent {
        id: NodeId,
        new_parent: NodeId,
        index: usize,
    },
    /// Create a new empty group as a child of the given parent.
    NewGroup { parent: NodeId },
    /// Wrap the selected nodes in a new group.
    GroupSelection { nodes: Vec<NodeId> },
    /// Dissolve a group, moving its children to the group's parent.
    Ungroup { group: NodeId },
    /// Delete a node.
    Delete { id: NodeId },
    /// Wrap the selected nodes in a new Boolean group with the given op.
    /// The resulting group renders as a single computed shape over its
    /// descendants.
    MakeBooleanGroup { nodes: Vec<NodeId>, op: BoolOp },
    /// Change the `BoolOp` on an existing Boolean group.
    SetBooleanOp { group: NodeId, op: BoolOp },
    /// Replace a Boolean group with a single Path node containing the
    /// baked computed geometry. Destroys the original operand children.
    FlattenBooleanGroup { group: NodeId },
}

/// A single flattened row in the structure panel tree view.
pub(crate) struct FlatRow {
    pub(crate) node_id: NodeId,
    pub(crate) parent_id: NodeId,
    /// Index of this node within its parent's children — needed so drop
    /// targets can address "insert before child #N of parent" precisely.
    pub(crate) index_in_parent: usize,
    /// Depth in the tree (root's direct children = 0).
    pub(crate) depth: u16,
    pub(crate) is_group: bool,
    pub(crate) is_defs: bool,
    /// Whether the group has children (always false for non-groups and defs).
    pub(crate) has_children: bool,
    /// Collapse state snapshot. Only meaningful for groups.
    pub(crate) collapsed: bool,
    pub(crate) icon: &'static str,
    pub(crate) label: String,
    pub(crate) visible: bool,
}

/// Walk the scene once and produce a flat list of rows to render. Collapsed
/// groups contribute only themselves; their descendants are omitted. This
/// is what makes virtualization possible: `ScrollArea::show_rows` can index
/// into the flat list by visible-row number.
pub(crate) fn flatten_tree(scene: &Scene, collapse: &HashMap<NodeId, bool>) -> Vec<FlatRow> {
    profiling::scope!("flatten_tree.walk");
    let mut out = Vec::new();
    let root = scene.root();
    if let Some(root_node) = scene.get(root) {
        let children: Vec<_> = root_node.children.clone();
        for (i, child) in children.into_iter().enumerate() {
            flatten_recurse(scene, collapse, child, root, i, 0, &mut out);
        }
    }
    out
}

fn flatten_recurse(
    scene: &Scene,
    collapse: &HashMap<NodeId, bool>,
    node_id: NodeId,
    parent_id: NodeId,
    index_in_parent: usize,
    depth: u16,
    out: &mut Vec<FlatRow>,
) {
    let Some(node) = scene.get(node_id) else {
        return;
    };

    let is_defs = matches!(&node.data, NodeData::Group { is_defs: true, .. });
    let is_group = matches!(&node.data, NodeData::Group { .. });
    // Preserve current behavior: defs is rendered as a leaf — its children
    // (gradients, patterns) aren't shown in the structure panel.
    let has_children = !node.children.is_empty() && !is_defs;
    let collapsed = collapse.get(&node_id).copied().unwrap_or(false);
    let (icon, label) = node_display(node);
    let visible = node.visible;
    let child_ids = if is_group && has_children && !collapsed {
        node.children.clone()
    } else {
        Vec::new()
    };

    out.push(FlatRow {
        node_id,
        parent_id,
        index_in_parent,
        depth,
        is_group,
        is_defs,
        has_children,
        collapsed,
        icon,
        label,
        visible,
    });

    for (i, child) in child_ids.into_iter().enumerate() {
        flatten_recurse(scene, collapse, child, node_id, i, depth + 1, out);
    }
}

/// Render a single virtualized row. Paints everything with direct
/// `painter.text()` and `painter.rect_filled()` calls rather than egui
/// widgets — we don't need full widget machinery for a glyph + hit-rect,
/// and the savings are material at scale.
///
/// Interaction rects:
///   - `chevron` (if group): toggles collapse
///   - `eye` (if not defs): toggles visibility
///   - `label_area`: row click (select) + drag source
///   - top 4px band: drop target (only active during a drag)
#[expect(clippy::too_many_arguments)]
pub(crate) fn render_virtual_row(
    ui: &mut egui::Ui,
    row: &FlatRow,
    row_height: f32,
    scene: &mut Scene,
    collapse: &mut HashMap<NodeId, bool>,
    selection: &mut SelectState,
    reorder_cmd: &mut ReorderCommand,
    structure_cmds: &mut StructureCommands,
    scene_dirty: &mut bool,
) {
    profiling::scope!("render_virtual_row");

    let node_selected = selection.is_node_selected(row.node_id);
    let text_color = if !row.visible || row.is_defs {
        ui.visuals().weak_text_color()
    } else if node_selected {
        egui::Color32::from_rgb(80, 160, 255)
    } else {
        ui.visuals().text_color()
    };

    // Reserve the full row rect (hover-only; clicks go to sub-rects below).
    let (row_rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_height),
        egui::Sense::hover(),
    );

    // Selection tint behind everything.
    if node_selected {
        ui.painter().rect_filled(
            row_rect,
            2.0,
            egui::Color32::from_rgba_premultiplied(40, 90, 160, 60),
        );
    }

    const INDENT_STEP: f32 = 12.0;
    const CHEVRON_W: f32 = 14.0;
    const EYE_W: f32 = 16.0;
    const GLYPH_GAP: f32 = 4.0;

    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let mut cursor_x = row_rect.left() + (row.depth as f32) * INDENT_STEP + 2.0;

    // Chevron (interactive) for groups with children.
    if row.is_group && row.has_children {
        let chevron_icon = if row.collapsed {
            egui_phosphor::regular::CARET_RIGHT
        } else {
            egui_phosphor::regular::CARET_DOWN
        };
        let chevron_rect = egui::Rect::from_min_size(
            egui::pos2(cursor_x, row_rect.top()),
            egui::vec2(CHEVRON_W, row_height),
        );
        let chevron_resp = ui.interact(
            chevron_rect,
            egui::Id::new(("chevron", row.node_id)),
            egui::Sense::click(),
        );
        ui.painter().text(
            chevron_rect.center(),
            egui::Align2::CENTER_CENTER,
            chevron_icon,
            font_id.clone(),
            text_color,
        );
        if chevron_resp.clicked() {
            let e = collapse.entry(row.node_id).or_insert(false);
            *e = !*e;
        }
    }
    cursor_x += CHEVRON_W + GLYPH_GAP;

    // Visibility eye (interactive) — defs can't be toggled.
    if !row.is_defs {
        let eye_icon = if row.visible {
            egui_phosphor::regular::EYE
        } else {
            egui_phosphor::regular::EYE_SLASH
        };
        let eye_color = if row.visible {
            ui.visuals().text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        let eye_rect = egui::Rect::from_min_size(
            egui::pos2(cursor_x, row_rect.top()),
            egui::vec2(EYE_W, row_height),
        );
        let eye_resp = ui.interact(
            eye_rect,
            egui::Id::new(("eye", row.node_id)),
            egui::Sense::click(),
        );
        ui.painter().text(
            eye_rect.center(),
            egui::Align2::CENTER_CENTER,
            eye_icon,
            font_id.clone(),
            eye_color,
        );
        if eye_resp.clicked()
            && let Some(n) = scene.get_mut(row.node_id)
        {
            n.visible = !n.visible;
            *scene_dirty = true;
        }
        let _ = eye_resp.on_hover_text(if row.visible { "Hide" } else { "Show" });
    }
    cursor_x += EYE_W + GLYPH_GAP;

    // Icon + label, painted directly.
    let label_text = format!("{} {}", row.icon, row.label);
    ui.painter().text(
        egui::pos2(cursor_x, row_rect.center().y),
        egui::Align2::LEFT_CENTER,
        &label_text,
        font_id,
        text_color,
    );

    // The "rest of the row" (everything right of the eye) is the row's
    // click + drag target.
    let label_area = egui::Rect::from_min_max(
        egui::pos2(cursor_x, row_rect.top()),
        row_rect.right_bottom(),
    );
    let row_response = ui.interact(
        label_area,
        egui::Id::new(("row_click", row.node_id)),
        egui::Sense::click_and_drag(),
    );

    if row_response.clicked() {
        let shift = ui.input(|i| i.modifiers.shift);
        let ctrl = ui.input(|i| i.modifiers.ctrl || i.modifiers.mac_cmd);
        structure_panel_click(selection, row.node_id, shift || ctrl);
    } else if row_response.drag_started() && !row.is_defs {
        egui::DragAndDrop::set_payload(ui.ctx(), row.node_id);
        ui.ctx()
            .set_dragged_id(egui::Id::new(("struct_drag", row.node_id)));
    }

    // Context menu on right-click anywhere in the row.
    row_response.context_menu(|ui| {
        node_context_menu(
            ui,
            row.node_id,
            row.is_group,
            row.is_defs,
            selection,
            structure_cmds,
        );
    });

    // Drop target at top band — only material during a drag. A drop here
    // means "insert before this row, as sibling #N of parent".
    if egui::DragAndDrop::has_any_payload(ui.ctx()) {
        let top_band = egui::Rect::from_min_size(row_rect.min, egui::vec2(row_rect.width(), 4.0));
        let drop_resp = ui.interact(
            top_band,
            egui::Id::new(("drop_slot", row.parent_id, row.index_in_parent)),
            egui::Sense::hover(),
        );
        let hovering = drop_resp.contains_pointer()
            && egui::DragAndDrop::has_payload_of_type::<NodeId>(ui.ctx());
        if hovering {
            ui.painter().hline(
                top_band.x_range(),
                top_band.center().y,
                egui::Stroke::new(2.0, egui::Color32::from_rgb(80, 160, 255)),
            );
            if ui.input(|i| i.pointer.any_released())
                && let Some(dragged_id) = egui::DragAndDrop::take_payload::<NodeId>(ui.ctx())
            {
                *reorder_cmd = Some((*dragged_id, row.parent_id, row.index_in_parent));
            }
        }
    }
}

/// Context menu for a scene node in the structure panel.
pub(crate) fn node_context_menu(
    ui: &mut egui::Ui,
    node_id: NodeId,
    is_group: bool,
    is_defs: bool,
    selection: &SelectState,
    structure_cmds: &mut StructureCommands,
) {
    if !is_defs {
        // "Add Group Inside" — only for groups
        if is_group
            && ui
                .button(format!(
                    "{} Add Group Inside",
                    egui_phosphor::regular::FOLDER_PLUS
                ))
                .clicked()
        {
            structure_cmds.push(StructureAction::NewGroup { parent: node_id });
            ui.close();
        }

        // "Group Selection" — when multiple nodes are selected
        if selection.selected_nodes.len() > 1
            && ui
                .button(format!(
                    "{} Group Selection",
                    egui_phosphor::regular::SELECTION_ALL
                ))
                .clicked()
        {
            structure_cmds.push(StructureAction::GroupSelection {
                nodes: selection.selected_nodes.clone(),
            });
            ui.close();
        }

        // "Ungroup" — only for non-defs groups
        if is_group
            && ui
                .button(format!("{} Ungroup", egui_phosphor::regular::FOLDER_MINUS))
                .clicked()
        {
            structure_cmds.push(StructureAction::Ungroup { group: node_id });
            ui.close();
        }

        ui.separator();

        // "Delete"
        if ui
            .button(format!("{} Delete", egui_phosphor::regular::TRASH))
            .clicked()
        {
            structure_cmds.push(StructureAction::Delete { id: node_id });
            ui.close();
        }
    }
}

/// Handle a click on a node in the structure panel.
///
/// With `additive` (Shift or Ctrl/Cmd held), the node is toggled in the
/// selection — added if not present, removed if already selected.
/// Without a modifier the selection is replaced with just this node.
pub(crate) fn structure_panel_click(selection: &mut SelectState, node_id: NodeId, additive: bool) {
    selection.selected.clear();
    selection.mode = vector_tools::SelectionMode::Object;

    if additive {
        // Toggle: remove if already selected, otherwise add
        if let Some(idx) = selection.selected_nodes.iter().position(|n| *n == node_id) {
            selection.selected_nodes.remove(idx);
        } else {
            selection.selected_nodes.push(node_id);
        }
    } else {
        selection.selected_nodes.clear();
        selection.selected_nodes.push(node_id);
    }
}

/// Apply a deferred structure-panel action to the scene.
pub(crate) fn apply_structure_action(action: StructureAction, state: &mut EditorState) {
    use vector_scene::Node;

    match action {
        StructureAction::Reparent {
            id,
            new_parent,
            index,
        } => {
            let cmd = Command::Reparent {
                id,
                new_parent,
                index,
            };
            state.history.execute(cmd, &mut state.scene);
        }
        StructureAction::NewGroup { parent } => {
            let cmd = Command::Insert {
                parent,
                index: None,
                node: Box::new(Node::group("Group")),
            };
            state.history.execute(cmd, &mut state.scene);
        }
        StructureAction::GroupSelection { nodes } => {
            if nodes.is_empty() {
                return;
            }
            // Find the common parent of all selected nodes.
            // Use the first node's parent as the group target.
            let Some(parent) = state.scene.parent(nodes[0]) else {
                return;
            };

            // Find the lowest index among selected nodes so the group
            // appears at the position of the first selected node.
            let mut min_index = usize::MAX;
            for &nid in &nodes {
                if state.scene.parent(nid) != Some(parent) {
                    // For simplicity, only group siblings. If they have
                    // different parents, just use the first node's parent.
                    continue;
                }
                if let Some(idx) = state.scene.child_index(nid) {
                    min_index = min_index.min(idx);
                }
            }
            if min_index == usize::MAX {
                min_index = 0;
            }

            // Insert the group first, then reparent each node into it.
            // We need stepwise execution because Batch doesn't expose
            // the inserted NodeId.
            let group_cmd = Command::Insert {
                parent,
                index: Some(min_index),
                node: Box::new(Node::group("Group")),
            };
            state.history.execute(group_cmd, &mut state.scene);

            // The new group is at `min_index` in `parent`'s children.
            let Some(parent_node) = state.scene.get(parent) else {
                return;
            };
            let Some(&group_id) = parent_node.children.get(min_index) else {
                return;
            };

            // Now reparent each selected node into the group.
            // Collect reparent commands into a batch so undo is atomic.
            let mut reparent_cmds = Vec::new();
            for (i, &nid) in nodes.iter().enumerate() {
                // Skip the defs node or root.
                if nid == state.scene.root() || nid == state.scene.defs() {
                    continue;
                }
                reparent_cmds.push(Command::Reparent {
                    id: nid,
                    new_parent: group_id,
                    index: i,
                });
            }
            if !reparent_cmds.is_empty() {
                state
                    .history
                    .execute(Command::Batch(reparent_cmds), &mut state.scene);
            }

            // Select the new group.
            state.select_state.selected_nodes.clear();
            state.select_state.selected_nodes.push(group_id);
        }
        StructureAction::Ungroup { group } => {
            let Some(group_node) = state.scene.get(group) else {
                return;
            };
            // Only ungroup actual groups (not defs).
            if !matches!(group_node.data, NodeData::Group { is_defs: false, .. }) {
                return;
            }
            let children: Vec<NodeId> = group_node.children.clone();
            if children.is_empty() {
                // Just delete the empty group.
                state
                    .history
                    .execute(Command::Delete { id: group }, &mut state.scene);
                return;
            }

            let Some(parent) = state.scene.parent(group) else {
                return;
            };
            let group_index = state.scene.child_index(group).unwrap_or(0);

            // Reparent each child to the group's parent, at sequential indices
            // starting from the group's current position.
            let mut reparent_cmds = Vec::new();
            for (i, &child) in children.iter().enumerate() {
                reparent_cmds.push(Command::Reparent {
                    id: child,
                    new_parent: parent,
                    index: group_index + i,
                });
            }
            reparent_cmds.push(Command::Delete { id: group });

            state
                .history
                .execute(Command::Batch(reparent_cmds), &mut state.scene);

            // Select the ungrouped children.
            state.select_state.selected_nodes = children;
        }
        StructureAction::Delete { id } => {
            if id == state.scene.root() || id == state.scene.defs() {
                return;
            }
            state
                .history
                .execute(Command::Delete { id }, &mut state.scene);
            state.select_state.selected_nodes.retain(|&n| n != id);
        }
        StructureAction::MakeBooleanGroup { nodes, op } => {
            if nodes.is_empty() {
                return;
            }
            let Some(parent) = state.scene.parent(nodes[0]) else {
                return;
            };

            // Pick an insertion index — the minimum index among the
            // selected siblings of `parent`.
            let mut min_index = usize::MAX;
            for &nid in &nodes {
                if state.scene.parent(nid) != Some(parent) {
                    continue;
                }
                if let Some(idx) = state.scene.child_index(nid) {
                    min_index = min_index.min(idx);
                }
            }
            if min_index == usize::MAX {
                min_index = 0;
            }

            // Inherit the style of the bottom-most Path descendant among
            // the selected nodes — that's the visual "base" the user
            // sees shine through after the op. Falls back to default.
            let inherited_style = inherit_style_for_boolean(&state.scene, &nodes);

            // 1) Insert a new group.
            let group_cmd = Command::Insert {
                parent,
                index: Some(min_index),
                node: Box::new(Node::group("Boolean")),
            };
            state.history.execute(group_cmd, &mut state.scene);
            let Some(parent_node) = state.scene.get(parent) else {
                return;
            };
            let Some(&group_id) = parent_node.children.get(min_index) else {
                return;
            };

            // 2) Reparent selected nodes into the group, preserving their
            //    z-order (earlier children = bottom, as in the scene).
            let mut batch: Vec<Command> = Vec::new();
            for (i, &nid) in nodes.iter().enumerate() {
                if nid == state.scene.root() || nid == state.scene.defs() {
                    continue;
                }
                batch.push(Command::Reparent {
                    id: nid,
                    new_parent: group_id,
                    index: i,
                });
            }
            // 3) Flip the group's kind to Boolean with the chosen op.
            batch.push(Command::SetGroupKind {
                id: group_id,
                kind: GroupKind::Boolean {
                    op,
                    style: inherited_style,
                },
            });
            if !batch.is_empty() {
                state
                    .history
                    .execute(Command::Batch(batch), &mut state.scene);
            }

            // Select the new group.
            state.select_state.selected_nodes.clear();
            state.select_state.selected_nodes.push(group_id);
        }
        StructureAction::SetBooleanOp { group, op } => {
            let Some(node) = state.scene.get(group) else {
                return;
            };
            // Keep the existing style; only swap the op.
            let NodeData::Group {
                kind: GroupKind::Boolean { style, .. },
                ..
            } = &node.data
            else {
                return;
            };
            let new_kind = GroupKind::Boolean {
                op,
                style: style.clone(),
            };
            state.history.execute(
                Command::SetGroupKind {
                    id: group,
                    kind: new_kind,
                },
                &mut state.scene,
            );
        }
        StructureAction::FlattenBooleanGroup { group } => {
            let Some(node) = state.scene.get(group) else {
                return;
            };
            let NodeData::Group {
                kind: GroupKind::Boolean { style, .. },
                ..
            } = &node.data
            else {
                return;
            };
            let style = style.clone();
            let label = if node.label.is_empty() {
                "Path".to_string()
            } else {
                node.label.clone()
            };
            let transform = node.transform;
            let computed = vector_bool::compute_boolean_group_path(&state.scene, group);
            if computed.subpaths.is_empty() {
                // Empty result — just delete the group.
                state
                    .history
                    .execute(Command::Delete { id: group }, &mut state.scene);
                state.select_state.selected_nodes.retain(|&n| n != group);
                return;
            }

            let Some(parent) = state.scene.parent(group) else {
                return;
            };
            let index = state.scene.child_index(group).unwrap_or(0);

            let mut path_node = Node::path(label, computed);
            path_node.transform = transform;
            if let NodeData::Path { style: s, .. } = &mut path_node.data {
                *s = style;
            }

            // Batch: delete the group, insert the baked path in its
            // place. Undo restores the full boolean structure.
            let batch = Command::Batch(vec![
                Command::Delete { id: group },
                Command::Insert {
                    parent,
                    index: Some(index),
                    node: Box::new(path_node),
                },
            ]);
            state.history.execute(batch, &mut state.scene);

            // Select the new path if we can find it.
            if let Some(parent_node) = state.scene.get(parent)
                && let Some(&new_id) = parent_node.children.get(index)
            {
                state.select_state.selected_nodes.clear();
                state.select_state.selected_nodes.push(new_id);
            }
        }
    }
}

/// Pick a style for a new Boolean group — the style of the bottom-most
/// Path descendant among `nodes`, or `Style::default()` if none.
fn inherit_style_for_boolean(scene: &Scene, nodes: &[NodeId]) -> Style {
    // `nodes` is assumed to be in the same order they appear as siblings.
    // The first (lowest-index) path descendant is the visual base.
    for &nid in nodes {
        if let Some(node) = scene.get(nid) {
            if let NodeData::Path { style, .. } = &node.data {
                return style.clone();
            }
            // Recurse into groups to find the first Path descendant.
            let mut found: Option<Style> = None;
            scene.walk_depth_first(nid, vector_geom::Affine::IDENTITY, &mut |_id, n, _w| {
                if found.is_some() {
                    return false;
                }
                if let NodeData::Path { style, .. } = &n.data {
                    found = Some(style.clone());
                    return false;
                }
                true
            });
            if let Some(s) = found {
                return s;
            }
        }
    }
    Style::default()
}
