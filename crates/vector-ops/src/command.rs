use vector_scene::{NodeId, Scene};

/// A reversible editing command. Each variant knows how to apply
/// itself and produce an undo command.
#[derive(Debug, Clone)]
pub enum Command {
    /// Insert a node as child of parent.
    Insert {
        parent: NodeId,
        node: vector_scene::Node,
    },
    /// Delete a node (and its subtree).
    Delete {
        id: NodeId,
    },
    /// Batch of commands applied atomically.
    Batch(Vec<Command>),
    // TODO: SetTransform, SetStyle, Reparent, SetPathData, etc.
}

impl Command {
    /// Apply this command to the scene, returning an undo command.
    pub fn apply(self, scene: &mut Scene) -> Option<Command> {
        match self {
            Command::Insert { parent, node } => {
                let id = scene.insert(parent, node)?;
                Some(Command::Delete { id })
            }
            Command::Delete { id } => {
                let removed = scene.remove(id);
                if removed.is_empty() {
                    return None;
                }
                // Undo: re-insert removed nodes.
                // For now, simplified — only handles single node.
                // TODO: handle full subtree re-insertion with correct parent relationships.
                let (_, node) = removed.into_iter().next()?;
                let parent = scene.root(); // TODO: remember original parent
                Some(Command::Insert { parent, node })
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
