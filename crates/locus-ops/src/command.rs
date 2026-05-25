use locus_geom::{Affine, Bounds, Path};
use locus_scene::{Gradient, GroupKind, NodeId, NodeSnapshot, Scene, Style, TextData};

/// A reversible editing command. Each variant knows how to apply
/// itself and produce an undo command.
#[derive(Debug, Clone)]
pub enum Command {
    /// Insert a node as child of parent at a specific index.
    Insert {
        parent: NodeId,
        index: Option<usize>,
        node: Box<locus_scene::Node>,
    },
    /// Re-insert a previously-deleted subtree (node + all descendants).
    InsertSubtree {
        parent: NodeId,
        index: usize,
        snapshot: Box<NodeSnapshot>,
    },
    /// Delete a node (and its subtree).
    Delete { id: NodeId },
    /// Replace the path data of a path node.
    SetPathData { id: NodeId, path: Path },
    /// Replace the style of a path node.
    SetStyle { id: NodeId, style: Style },
    /// Replace the text data of a text node.
    SetTextData { id: NodeId, text: TextData },
    /// Replace the `Gradient` carried by a `NodeData::Paint(Paint::Gradient)`
    /// node. Mirrors `SetPathData`: atomic value swap, undo is the inverse
    /// `SetGradient` with the previous value.
    SetGradient { id: NodeId, gradient: Gradient },
    /// Replace the transform of a node.
    SetTransform { id: NodeId, transform: Affine },
    /// Replace the `GroupKind` of a group node — used to convert between
    /// regular and boolean groups, or to switch the boolean op / style.
    /// Fails silently (returns None) if the target is not a Group.
    SetGroupKind { id: NodeId, kind: GroupKind },
    /// Move a node to a new parent at a given index.
    Reparent {
        id: NodeId,
        new_parent: NodeId,
        index: usize,
    },
    /// Replace the document viewBox (page rectangle). Undo restores the
    /// previous viewBox. Used by "Fit Page to Content" and any future
    /// document-properties UI that lets the user resize/reposition the page.
    SetViewBox { bounds: Bounds },
    /// Dissolve a group, moving its children to the group's parent while
    /// folding the group's transform into each child so they stay put.
    /// The inverse is `Regroup`, which captures enough state to restore
    /// the original group (with its original NodeId-bearing children) on
    /// undo. Lives as a dedicated command rather than a `Batch` because
    /// the auto-derived batch undo can't refer to the deleted group's
    /// NodeId after it's removed from the scene's SlotMap.
    Ungroup { group: NodeId },
    /// Inverse of `Ungroup`: re-create the group from `snapshot` at
    /// `(parent, index)`, then move each listed child back into it at
    /// its original index with its original transform.
    Regroup {
        snapshot: Box<NodeSnapshot>,
        parent: NodeId,
        index: usize,
        /// `(child_id, original_transform, original_index_in_group)`.
        children: Vec<(NodeId, Affine, usize)>,
    },
    /// Batch of commands applied atomically.
    Batch(Vec<Command>),
}

impl Command {
    /// Apply this command to the scene, returning an undo command.
    pub fn apply(self, scene: &mut Scene) -> Option<Command> {
        match self {
            Command::Insert {
                parent,
                index,
                node,
            } => {
                let id = scene.insert_at(parent, *node, index)?;
                Some(Command::Delete { id })
            }
            Command::InsertSubtree {
                parent,
                index,
                snapshot,
            } => {
                let id = scene.insert_subtree(parent, index, *snapshot)?;
                Some(Command::Delete { id })
            }
            Command::Delete { id } => {
                let parent = scene.parent(id)?;
                let index = scene.child_index(id).unwrap_or(0);
                let snapshot = scene.snapshot_subtree(id)?;
                let removed = scene.remove(id);
                if removed.is_empty() {
                    return None;
                }
                Some(Command::InsertSubtree {
                    parent,
                    index,
                    snapshot: Box::new(snapshot),
                })
            }
            Command::SetPathData { id, path } => {
                let old = scene.set_path_data(id, path)?;
                Some(Command::SetPathData { id, path: old })
            }
            Command::SetStyle { id, style } => {
                let old = scene.set_style(id, style)?;
                Some(Command::SetStyle { id, style: old })
            }
            Command::SetTextData { id, text } => {
                let old = scene.set_text_data(id, text)?;
                Some(Command::SetTextData { id, text: old })
            }
            Command::SetGradient { id, gradient } => {
                let old = scene.set_gradient(id, gradient)?;
                Some(Command::SetGradient { id, gradient: old })
            }
            Command::SetTransform { id, transform } => {
                let old = scene.set_transform(id, transform)?;
                Some(Command::SetTransform { id, transform: old })
            }
            Command::SetGroupKind { id, kind } => {
                let old = scene.set_group_kind(id, kind)?;
                Some(Command::SetGroupKind { id, kind: old })
            }
            Command::Reparent {
                id,
                new_parent,
                index,
            } => {
                // Capture old parent and index for undo.
                let old_parent = scene.parent(id)?;
                let old_index = scene.child_index(id).unwrap_or(0);
                if scene.reparent(id, new_parent, index) {
                    Some(Command::Reparent {
                        id,
                        new_parent: old_parent,
                        index: old_index,
                    })
                } else {
                    None
                }
            }
            Command::SetViewBox { bounds } => {
                let old = scene.view_box();
                if old == bounds {
                    return None;
                }
                scene.set_view_box(bounds);
                Some(Command::SetViewBox { bounds: old })
            }
            Command::Ungroup { group } => {
                // Capture everything we need to rebuild the group on undo,
                // *before* mutating anything.
                let group_node = scene.get(group)?;
                if !matches!(group_node.data, locus_scene::NodeData::Group { .. }) {
                    return None;
                }
                let group_transform = group_node.transform;
                let parent = scene.parent(group)?;
                let group_index = scene.child_index(group).unwrap_or(0);
                let child_ids: Vec<NodeId> = group_node.children.clone();

                // Original (id, transform, index-in-group) so Regroup
                // can put each child back exactly where it was.
                let mut original_children: Vec<(NodeId, Affine, usize)> =
                    Vec::with_capacity(child_ids.len());
                for (i, &cid) in child_ids.iter().enumerate() {
                    if let Some(cn) = scene.get(cid) {
                        original_children.push((cid, cn.transform, i));
                    }
                }

                // Fold the group's transform into each child so visible
                // position is preserved across the ungroup.
                if !group_transform.is_identity() {
                    for &(cid, orig_t, _) in &original_children {
                        let composed = group_transform.then(orig_t);
                        if composed != orig_t {
                            scene.set_transform(cid, composed);
                        }
                    }
                }

                // Move children out to the group's parent at sequential
                // indices starting from the group's slot.
                for (i, &cid) in child_ids.iter().enumerate() {
                    scene.reparent(cid, parent, group_index + i);
                }

                // Capture the now-empty group as a snapshot, then remove
                // it. Snapshot has zero children at this point.
                let snapshot = scene.snapshot_subtree(group)?;
                scene.remove(group);

                Some(Command::Regroup {
                    snapshot: Box::new(snapshot),
                    parent,
                    index: group_index,
                    children: original_children,
                })
            }
            Command::Regroup {
                snapshot,
                parent,
                index,
                children,
            } => {
                // Re-create the empty group at its original slot. The
                // resurrected group gets a fresh NodeId — that's fine
                // because Regroup's redo (Ungroup) will look it up by
                // the *new* id.
                let new_group_id = scene.insert_subtree(parent, index, *snapshot)?;
                // Reparent each original child back into the new group at
                // its recorded index, restoring its original transform.
                for (cid, orig_t, orig_idx) in children {
                    scene.reparent(cid, new_group_id, orig_idx);
                    scene.set_transform(cid, orig_t);
                }
                Some(Command::Ungroup {
                    group: new_group_id,
                })
            }
            Command::Batch(cmds) => {
                let mut undos: Vec<Command> = Vec::new();
                for cmd in cmds {
                    if let Some(undo) = cmd.apply(scene) {
                        undos.push(undo);
                    }
                }
                undos.reverse(); // undo in reverse order
                Some(Command::Batch(undos))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use locus_geom::{Path, Point, Segment, SubPath, VertexMode};
    use locus_scene::{Node, NodeData};

    fn make_test_path() -> Path {
        let mut path = Path::new();
        path.subpaths.push(SubPath {
            start: Point::new(0.0, 0.0),
            segments: vec![Segment::Line {
                to: Point::new(100.0, 100.0),
            }],
            closed: false,
            vertex_modes: vec![VertexMode::Corner; 2],
        });
        path
    }

    #[test]
    fn insert_then_undo_removes() {
        let mut scene = Scene::new();
        let root = scene.root();
        let node = Node::path("test", make_test_path());

        let cmd = Command::Insert {
            parent: root,
            index: None,
            node: Box::new(node),
        };
        let undo = cmd.apply(&mut scene).expect("insert should return undo");

        // The node should exist in the scene.
        let root_node = scene.get(root).unwrap();
        assert!(root_node.children.len() > 1); // defs + new node

        // Applying the undo (Delete) should remove it.
        let redo = undo.apply(&mut scene).expect("undo should return redo");
        let root_node = scene.get(root).unwrap();
        // Only defs should remain.
        assert_eq!(root_node.children.len(), 1);

        // Applying the redo (InsertSubtree) should re-insert it.
        redo.apply(&mut scene).expect("redo should return undo");
        let root_node = scene.get(root).unwrap();
        assert!(root_node.children.len() > 1);
    }

    #[test]
    fn delete_preserves_parent_and_index() {
        let mut scene = Scene::new();
        let root = scene.root();

        let a = scene
            .insert(root, Node::path("a", make_test_path()))
            .unwrap();
        let _b = scene
            .insert(root, Node::path("b", make_test_path()))
            .unwrap();

        // Delete 'a' (index 1, after defs at 0).
        let cmd = Command::Delete { id: a };
        let undo = cmd.apply(&mut scene).expect("delete should return undo");

        // 'a' should be gone.
        assert!(scene.get(a).is_none());

        // Undo should re-insert at the same position.
        undo.apply(&mut scene).expect("undo should work");

        let root_node = scene.get(root).unwrap();
        assert_eq!(root_node.children.len(), 3); // defs + a + b
        // The re-inserted node should be at index 1.
        let reinserted = root_node.children[1];
        assert_eq!(scene.get(reinserted).unwrap().label, "a");
    }

    #[test]
    fn ungroup_then_undo_restores_group_and_transforms() {
        let mut scene = Scene::new();
        let root = scene.root();

        // Group at translate(10, 20) with two path children at their own
        // translations. After ungroup the children should sit at
        // (10+1, 20+2) and (10+3, 20+4) under root; after undo they
        // should be back inside the group at their original transforms.
        let mut group = Node::group("g");
        group.transform = Affine::translate(10.0, 20.0);
        let group_id = scene.insert(root, group).unwrap();

        let mut child_a = Node::path("a", make_test_path());
        child_a.transform = Affine::translate(1.0, 2.0);
        let a = scene.insert(group_id, child_a).unwrap();

        let mut child_b = Node::path("b", make_test_path());
        child_b.transform = Affine::translate(3.0, 4.0);
        let b = scene.insert(group_id, child_b).unwrap();

        let orig_a_transform = scene.get(a).unwrap().transform;
        let orig_b_transform = scene.get(b).unwrap().transform;
        let orig_root_kids = scene.get(root).unwrap().children.clone();

        // Ungroup.
        let undo = Command::Ungroup { group: group_id }
            .apply(&mut scene)
            .expect("ungroup should return undo");

        // Group is gone; both children are reparented to root with
        // composed transforms.
        assert!(scene.get(group_id).is_none());
        assert_eq!(scene.parent(a), Some(root));
        assert_eq!(scene.parent(b), Some(root));
        assert_eq!(scene.get(a).unwrap().transform.tx, 11.0);
        assert_eq!(scene.get(a).unwrap().transform.ty, 22.0);
        assert_eq!(scene.get(b).unwrap().transform.tx, 13.0);
        assert_eq!(scene.get(b).unwrap().transform.ty, 24.0);

        // Undo: children restored inside a fresh group at the original
        // slot, with their original transforms intact.
        let redo = undo.apply(&mut scene).expect("undo should return redo");

        let root_kids = scene.get(root).unwrap().children.clone();
        assert_eq!(root_kids.len(), orig_root_kids.len());
        // Children of the resurrected group are still `a` and `b` with
        // their original transforms.
        let Command::Ungroup { group: new_group } = redo else {
            panic!("redo of Regroup should be an Ungroup, got {redo:?}");
        };
        let new_group_kids = scene.get(new_group).unwrap().children.clone();
        assert_eq!(new_group_kids, vec![a, b]);
        assert_eq!(scene.get(a).unwrap().transform, orig_a_transform);
        assert_eq!(scene.get(b).unwrap().transform, orig_b_transform);
    }

    #[test]
    fn set_path_data_roundtrip() {
        let mut scene = Scene::new();
        let root = scene.root();
        let original_path = make_test_path();
        let node_id = scene
            .insert(root, Node::path("test", original_path.clone()))
            .unwrap();

        // Replace with a different path.
        let mut new_path = Path::new();
        new_path.subpaths.push(SubPath {
            start: Point::new(50.0, 50.0),
            segments: vec![Segment::Line {
                to: Point::new(200.0, 200.0),
            }],
            closed: true,
            vertex_modes: vec![VertexMode::Corner; 2],
        });

        let cmd = Command::SetPathData {
            id: node_id,
            path: new_path.clone(),
        };
        let undo = cmd.apply(&mut scene).expect("set_path should return undo");

        // Path should be the new one.
        let node = scene.get(node_id).unwrap();
        let NodeData::Path { ref path, .. } = node.data else {
            panic!("expected path node");
        };
        assert_eq!(path.subpaths[0].start, Point::new(50.0, 50.0));

        // Undo should restore the original.
        undo.apply(&mut scene).expect("undo should work");
        let node = scene.get(node_id).unwrap();
        let NodeData::Path { ref path, .. } = node.data else {
            panic!("expected path node");
        };
        assert_eq!(path.subpaths[0].start, Point::new(0.0, 0.0));
    }

    #[test]
    fn set_style_roundtrip() {
        let mut scene = Scene::new();
        let root = scene.root();
        let node_id = scene
            .insert(root, Node::path("test", make_test_path()))
            .unwrap();

        // Get the current style.
        let original_style = match &scene.get(node_id).unwrap().data {
            NodeData::Path { style, .. } => style.clone(),
            _ => panic!("expected path node"),
        };

        // Change fill to none.
        let mut new_style = original_style.clone();
        new_style.fill = None;

        let cmd = Command::SetStyle {
            id: node_id,
            style: new_style,
        };
        let undo = cmd.apply(&mut scene).expect("set_style should return undo");

        // Fill should be None now.
        let node = scene.get(node_id).unwrap();
        let NodeData::Path { style, .. } = &node.data else {
            panic!("expected path node");
        };
        assert!(style.fill.is_none());

        // Undo should restore the original fill.
        undo.apply(&mut scene).expect("undo should work");
        let node = scene.get(node_id).unwrap();
        let NodeData::Path { style, .. } = &node.data else {
            panic!("expected path node");
        };
        assert!(style.fill.is_some());
    }

    #[test]
    fn set_group_kind_roundtrip() {
        use locus_scene::{BoolOp, GroupKind};
        let mut scene = Scene::new();
        let root = scene.root();
        let group_id = scene.insert(root, Node::group("g")).unwrap();

        // Convert to a Boolean(Union) group.
        let new_kind = GroupKind::Boolean {
            op: BoolOp::Union,
            style: Style::default(),
        };
        let cmd = Command::SetGroupKind {
            id: group_id,
            kind: new_kind,
        };
        let undo = cmd
            .apply(&mut scene)
            .expect("set_group_kind should succeed");

        let node = scene.get(group_id).unwrap();
        let NodeData::Group { kind, .. } = &node.data else {
            panic!("expected group");
        };
        assert!(matches!(kind, GroupKind::Boolean { .. }));

        // Undo restores Regular.
        undo.apply(&mut scene).expect("undo should work");
        let node = scene.get(group_id).unwrap();
        let NodeData::Group { kind, .. } = &node.data else {
            panic!("expected group");
        };
        assert!(matches!(kind, GroupKind::Regular));
    }

    #[test]
    fn set_group_kind_on_non_group_is_none() {
        let mut scene = Scene::new();
        let root = scene.root();
        let path_id = scene
            .insert(root, Node::path("p", make_test_path()))
            .unwrap();

        let cmd = Command::SetGroupKind {
            id: path_id,
            kind: locus_scene::GroupKind::Regular,
        };
        assert!(cmd.apply(&mut scene).is_none());
    }
}
