//! Helpers shared by the live-preview dialogs (raster trace + LLM SVG
//! generation). Both flows want the same shape:
//!
//! 1. Run a background worker that produces a fresh `Scene`.
//! 2. On result, swap that scene's contents into a single "preview group"
//!    node in the live scene — creating it on first success, replacing
//!    its children on every later success.
//! 3. On Apply, snapshot the preview group as one undoable
//!    `Command::InsertSubtree`. On Cancel, drop it silently.
//!
//! The two helpers below cover the swap-children half. The Apply/Cancel
//! halves stay in each dialog because they touch dialog-specific state
//! (param structs, conversation logs).

use locus_render::Renderer;
use locus_scene::{NodeId, Scene};

/// Action returned by a non-modal preview dialog's `show` function. The
/// caller (in `app.rs::draw_frame`) interprets it as Apply / Cancel /
/// keep-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DialogAction {
    /// Keep the dialog open and the preview group live.
    None,
    /// Commit the preview group as a single undoable insert and close.
    Apply,
    /// Discard the preview group (no undo entry) and close.
    Cancel,
}

/// Splice `produced`'s contents into the preview group `*group_slot` in
/// `scene`. On the first call (when `*group_slot` is `None`) a new group
/// is created at the scene root; on subsequent calls the existing group's
/// children are replaced.
///
/// The `root_label` is used both as the group's display name on first
/// creation and as the inner-merge label on later updates (the inner
/// label is hidden by `lift_single_child`, but `merge_as_group` requires
/// one).
///
/// Returns the NodeId of the preview group after the call (which equals
/// `*group_slot` on success).
pub(crate) fn replace_children(
    scene: &mut Scene,
    produced: &Scene,
    group_slot: &mut Option<NodeId>,
    renderer: &mut Renderer,
    root_label: &str,
) -> Option<NodeId> {
    let existing = group_slot.filter(|id| scene.get(*id).is_some());

    let group_id = match existing {
        Some(group_id) => {
            // Empty the preview group's existing children. We remove
            // them rather than swapping the group so the group's own
            // NodeId stays stable across re-runs (preserves
            // tessellation cache keys, selection, etc.).
            let children: Vec<NodeId> = scene
                .get(group_id)
                .map(|n| n.children.clone())
                .unwrap_or_default();
            for child in children {
                scene.remove(child);
            }
            // Merge into the existing group.
            locus_scene::merge_as_group(scene, group_id, produced, root_label);
            // `merge_as_group` always wraps content in a new group;
            // flatten that nesting away so the structure panel doesn't
            // show a redundant level.
            lift_single_child(scene, group_id);
            group_id
        }
        None => {
            // First successful result — create the preview group at the
            // scene root.
            let parent = scene.root();
            let id = locus_scene::merge_as_group(scene, parent, produced, root_label)
                .expect("merge into root must succeed");
            *group_slot = Some(id);
            id
        }
    };

    renderer.mark_dirty();
    Some(group_id)
}

/// `merge_as_group` always wraps content in a new group. When we're
/// merging into an *existing* preview group, that produces an extra
/// nested layer — flatten it by moving the inner group's children up
/// and removing the inner group. Idempotent if the parent has zero
/// children or more than one.
pub(crate) fn lift_single_child(scene: &mut Scene, parent: NodeId) {
    let Some(parent_node) = scene.get(parent) else {
        return;
    };
    if parent_node.children.len() != 1 {
        return;
    }
    let inner = parent_node.children[0];
    let inner_children: Vec<NodeId> = match scene.get(inner) {
        Some(n) => n.children.clone(),
        None => return,
    };
    for (i, child) in inner_children.iter().enumerate() {
        scene.reparent(*child, parent, i);
    }
    scene.remove(inner);
}
