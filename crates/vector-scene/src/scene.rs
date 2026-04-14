use serde::{Deserialize, Serialize};
use slotmap::SlotMap;

use crate::node::Node;

// SlotMap's DefaultKey gives us stable, generational IDs for free:
// - O(1) lookup
// - IDs survive insertions/deletions of other nodes
// - Stale IDs (pointing at deleted nodes) are detected automatically
slotmap::new_key_type! {
    /// Stable identifier for a node in the scene graph.
    pub struct NodeId;
}

/// The scene: a flat pool of nodes with a designated root.
/// Parent-child relationships are stored inside each node (children vec),
/// with an auxiliary parent lookup for walking up the tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    nodes: SlotMap<NodeId, Node>,
    /// Parent lookup: child -> parent. Root has no entry.
    parents: slotmap::SecondaryMap<NodeId, NodeId>,
    /// The root group node. Always exists.
    root: NodeId,
    /// The defs group (non-rendered). Child of root, always exists.
    defs: NodeId,
}

impl Scene {
    /// Create a new empty scene with a root group and a defs group.
    pub fn new() -> Self {
        let mut nodes = SlotMap::with_key();
        let mut parents = slotmap::SecondaryMap::new();

        let root = nodes.insert(Node::group("root"));
        let defs = nodes.insert(Node {
            label: "defs".into(),
            data: crate::node::NodeData::Group { is_defs: true },
            ..Node::group("")
        });

        // defs is a child of root
        nodes[root].children.push(defs);
        parents.insert(defs, root);

        Self {
            nodes,
            parents,
            root,
            defs,
        }
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn defs(&self) -> NodeId {
        self.defs
    }

    /// Get a node by ID.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Get a mutable reference to a node.
    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(id)
    }

    /// Get the parent of a node.
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.parents.get(id).copied()
    }

    /// Insert a new node as a child of `parent`. Returns the new node's ID.
    pub fn insert(&mut self, parent: NodeId, node: Node) -> Option<NodeId> {
        if !self.nodes.contains_key(parent) {
            return None;
        }
        let id = self.nodes.insert(node);
        self.nodes[parent].children.push(id);
        self.parents.insert(id, parent);
        Some(id)
    }

    /// Remove a node and all its descendants. Returns the removed nodes.
    pub fn remove(&mut self, id: NodeId) -> Vec<(NodeId, Node)> {
        if id == self.root || id == self.defs {
            return Vec::new(); // can't remove structural nodes
        }
        let mut removed = Vec::new();
        self.remove_recursive(id, &mut removed);

        // Remove from parent's children list
        if let Some(parent_id) = self.parents.remove(id)
            && let Some(parent) = self.nodes.get_mut(parent_id)
        {
            parent.children.retain(|c| *c != id);
        }
        removed
    }

    fn remove_recursive(&mut self, id: NodeId, out: &mut Vec<(NodeId, Node)>) {
        if let Some(node) = self.nodes.remove(id) {
            let children: Vec<NodeId> = node.children.clone();
            out.push((id, node));
            for child in children {
                self.parents.remove(child);
                self.remove_recursive(child, out);
            }
        }
    }

    /// Move a node to be a child of a new parent at a given index.
    pub fn reparent(&mut self, id: NodeId, new_parent: NodeId, index: usize) -> bool {
        if id == self.root || id == self.defs || !self.nodes.contains_key(new_parent) {
            return false;
        }

        // Remove from old parent
        if let Some(old_parent) = self.parents.get(id).copied()
            && let Some(parent) = self.nodes.get_mut(old_parent)
        {
            parent.children.retain(|c| *c != id);
        }

        // Add to new parent
        let children = &mut self.nodes[new_parent].children;
        let idx = index.min(children.len());
        children.insert(idx, id);
        self.parents.insert(id, new_parent);
        true
    }

    /// Iterate over all nodes.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes.iter()
    }

    /// Walk the tree depth-first from a given root, calling `f` for each node
    /// with its accumulated world transform.
    pub fn walk_depth_first(
        &self,
        from: NodeId,
        parent_transform: vector_geom::Affine,
        f: &mut impl FnMut(NodeId, &Node, vector_geom::Affine),
    ) {
        let Some(node) = self.get(from) else {
            return;
        };
        let world = parent_transform.then(node.transform);
        f(from, node, world);
        // Clone children to avoid borrow issues
        let children: Vec<NodeId> = node.children.clone();
        for child in children {
            self.walk_depth_first(child, world, f);
        }
    }
}

impl Scene {
    /// Compute the bounding box of all visible path content (skipping defs).
    pub fn content_bounds(&self) -> vector_geom::Bounds {
        let mut bounds = vector_geom::Bounds::EMPTY;
        self.walk_depth_first(
            self.root,
            vector_geom::Affine::IDENTITY,
            &mut |_id, node, _world| {
                if !node.visible {
                    return;
                }
                if let crate::node::NodeData::Path { ref path, .. } = node.data {
                    let path_bounds = path.bounding_box();
                    if !path_bounds.is_empty() {
                        bounds = bounds.union(path_bounds);
                    }
                }
            },
        );
        bounds
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}
