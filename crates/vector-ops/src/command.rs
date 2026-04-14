use vector_geom::{Affine, Path};
use vector_scene::{NodeId, NodeSnapshot, Scene, Style};

/// A reversible editing command. Each variant knows how to apply
/// itself and produce an undo command.
#[derive(Debug, Clone)]
pub enum Command {
    /// Insert a node as child of parent at a specific index.
    Insert {
        parent: NodeId,
        index: Option<usize>,
        node: Box<vector_scene::Node>,
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
    /// Replace the transform of a node.
    SetTransform { id: NodeId, transform: Affine },
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
                let node = scene.get_mut(id)?;
                let vector_scene::NodeData::Path {
                    path: ref mut current,
                    ..
                } = node.data
                else {
                    return None;
                };
                let old = std::mem::replace(current, path);
                Some(Command::SetPathData { id, path: old })
            }
            Command::SetStyle { id, style } => {
                let node = scene.get_mut(id)?;
                let vector_scene::NodeData::Path {
                    style: ref mut current,
                    ..
                } = node.data
                else {
                    return None;
                };
                let old = std::mem::replace(current, style);
                Some(Command::SetStyle { id, style: old })
            }
            Command::SetTransform { id, transform } => {
                let node = scene.get_mut(id)?;
                let old = std::mem::replace(&mut node.transform, transform);
                Some(Command::SetTransform { id, transform: old })
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
    use vector_geom::{Path, Point, Segment, SubPath};
    use vector_scene::{Node, NodeData};

    fn make_test_path() -> Path {
        let mut path = Path::new();
        path.subpaths.push(SubPath {
            start: Point::new(0.0, 0.0),
            segments: vec![Segment::Line {
                to: Point::new(100.0, 100.0),
            }],
            closed: false,
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
}
