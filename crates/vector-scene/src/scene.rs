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

    /// Insert a new node as a child of `parent` (appended at the end).
    /// Returns the new node's ID.
    pub fn insert(&mut self, parent: NodeId, node: Node) -> Option<NodeId> {
        self.insert_at(parent, node, None)
    }

    /// Insert a new node as a child of `parent` at a specific index.
    /// If `index` is `None`, the node is appended at the end.
    pub fn insert_at(
        &mut self,
        parent: NodeId,
        node: Node,
        index: Option<usize>,
    ) -> Option<NodeId> {
        if !self.nodes.contains_key(parent) {
            return None;
        }
        let id = self.nodes.insert(node);
        let children = &mut self.nodes[parent].children;
        match index {
            Some(idx) => children.insert(idx.min(children.len()), id),
            None => children.push(id),
        }
        self.parents.insert(id, parent);
        Some(id)
    }

    /// Returns the index of `id` within its parent's children list.
    pub fn child_index(&self, id: NodeId) -> Option<usize> {
        let parent_id = self.parents.get(id).copied()?;
        let parent = self.nodes.get(parent_id)?;
        parent.children.iter().position(|c| *c == id)
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
    /// Returns true if `ancestor` is an ancestor of `descendant`
    /// (i.e. `descendant` is somewhere in the subtree of `ancestor`).
    /// A node is NOT considered its own ancestor.
    pub fn is_ancestor(&self, ancestor: NodeId, descendant: NodeId) -> bool {
        let mut current = descendant;
        while let Some(parent) = self.parents.get(current).copied() {
            if parent == ancestor {
                return true;
            }
            current = parent;
        }
        false
    }

    /// Compute the accumulated world transform for a node by walking up
    /// the parent chain. Returns `Affine::IDENTITY` for the root.
    pub fn world_transform(&self, id: NodeId) -> vector_geom::Affine {
        let mut chain = Vec::new();
        let mut current = id;
        while let Some(parent) = self.parents.get(current).copied() {
            chain.push(current);
            current = parent;
        }
        // `current` is now the root (no parent entry). Build the transform
        // from root down to `id`.
        let mut xform = self
            .nodes
            .get(current)
            .map_or(vector_geom::Affine::IDENTITY, |n| n.transform);
        for &node_id in chain.iter().rev() {
            if let Some(node) = self.nodes.get(node_id) {
                xform = xform.then(node.transform);
            }
        }
        xform
    }

    /// Compute the accumulated world transform of a node's *parent chain*,
    /// excluding the node's own transform. Useful for converting a world-space
    /// delta into the local space where the node's transform operates.
    pub fn parent_world_transform(&self, id: NodeId) -> vector_geom::Affine {
        let parent_id = match self.parents.get(id) {
            Some(&p) => p,
            None => return vector_geom::Affine::IDENTITY,
        };
        self.world_transform(parent_id)
    }

    /// Compute the bounding box of all visible content (paths and text),
    /// skipping defs, taking group transforms into account. Includes the
    /// visible stroke area around each shape.
    pub fn content_bounds(&self) -> vector_geom::Bounds {
        let mut bounds = vector_geom::Bounds::EMPTY;
        self.walk_depth_first(
            self.root,
            vector_geom::Affine::IDENTITY,
            &mut |_id, node, world| {
                if !node.visible {
                    return;
                }
                let node_bounds = node.data.visual_bounds(world);
                if !node_bounds.is_empty() {
                    bounds = bounds.union(node_bounds);
                }
            },
        );
        bounds
    }
}

/// A snapshot of a node and all its descendants, stored as an owned tree
/// of `Node` values (no `NodeId` references). Used for undo of subtree
/// deletion — the entire subtree can be re-inserted with fresh IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSnapshot {
    /// The node itself (with `children` vec cleared — child info is in `children` below).
    pub node: Node,
    /// Recursive snapshots of each child, in order.
    pub children: Vec<NodeSnapshot>,
}

impl Scene {
    /// Capture a recursive snapshot of the subtree rooted at `id`.
    pub fn snapshot_subtree(&self, id: NodeId) -> Option<NodeSnapshot> {
        let node = self.nodes.get(id)?;
        let child_snapshots: Vec<NodeSnapshot> = node
            .children
            .iter()
            .filter_map(|child_id| self.snapshot_subtree(*child_id))
            .collect();
        let mut node_clone = node.clone();
        node_clone.children.clear(); // children are stored in the snapshot tree
        Some(NodeSnapshot {
            node: node_clone,
            children: child_snapshots,
        })
    }

    /// Re-insert a previously-captured subtree as a child of `parent` at `index`.
    /// Returns the new root `NodeId` of the re-inserted subtree.
    pub fn insert_subtree(
        &mut self,
        parent: NodeId,
        index: usize,
        snapshot: NodeSnapshot,
    ) -> Option<NodeId> {
        let root_id = self.insert_at(parent, snapshot.node, Some(index))?;
        for child_snap in snapshot.children {
            // Children are appended in order (None = append)
            self.insert_subtree_recursive(root_id, child_snap);
        }
        Some(root_id)
    }

    fn insert_subtree_recursive(&mut self, parent: NodeId, snapshot: NodeSnapshot) {
        if let Some(id) = self.insert(parent, snapshot.node) {
            for child_snap in snapshot.children {
                self.insert_subtree_recursive(id, child_snap);
            }
        }
    }
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_geom::{Path, Point, Segment, SubPath, VertexMode};

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
    fn insert_at_specific_index() {
        let mut scene = Scene::new();
        let root = scene.root();

        let a = scene
            .insert(root, Node::path("a", make_test_path()))
            .unwrap();
        let b = scene
            .insert(root, Node::path("b", make_test_path()))
            .unwrap();

        // Insert c at index 1 (between a and b). Root children are [defs, a, b].
        let c = scene
            .insert_at(root, Node::path("c", make_test_path()), Some(1))
            .unwrap();

        let root_node = scene.get(root).unwrap();
        // Children should be: defs, c, a, b
        assert_eq!(root_node.children[1], c);
        assert_eq!(root_node.children[2], a);
        assert_eq!(root_node.children[3], b);
    }

    #[test]
    fn child_index_returns_correct_position() {
        let mut scene = Scene::new();
        let root = scene.root();

        let a = scene
            .insert(root, Node::path("a", make_test_path()))
            .unwrap();
        let b = scene
            .insert(root, Node::path("b", make_test_path()))
            .unwrap();

        // defs is at 0, a at 1, b at 2.
        assert_eq!(scene.child_index(a), Some(1));
        assert_eq!(scene.child_index(b), Some(2));
        assert_eq!(scene.child_index(scene.defs()), Some(0));
    }

    #[test]
    fn snapshot_and_reinsert_subtree() {
        let mut scene = Scene::new();
        let root = scene.root();

        let group = scene.insert(root, Node::group("g")).unwrap();
        let _child_a = scene
            .insert(group, Node::path("a", make_test_path()))
            .unwrap();
        let _child_b = scene
            .insert(group, Node::path("b", make_test_path()))
            .unwrap();

        // Snapshot the group subtree.
        let snapshot = scene.snapshot_subtree(group).unwrap();
        assert_eq!(snapshot.node.label, "g");
        assert_eq!(snapshot.children.len(), 2);
        assert_eq!(snapshot.children[0].node.label, "a");
        assert_eq!(snapshot.children[1].node.label, "b");

        // Remove the group.
        let parent = scene.parent(group).unwrap();
        let index = scene.child_index(group).unwrap();
        scene.remove(group);

        // The group and its children should be gone.
        assert!(scene.get(group).is_none());

        // Re-insert from snapshot.
        let new_group = scene.insert_subtree(parent, index, snapshot).unwrap();
        let new_node = scene.get(new_group).unwrap();
        assert_eq!(new_node.label, "g");
        assert_eq!(new_node.children.len(), 2);

        // Children should be path nodes with correct labels.
        let child_labels: Vec<&str> = new_node
            .children
            .iter()
            .map(|&id| scene.get(id).unwrap().label.as_str())
            .collect();
        assert_eq!(child_labels, vec!["a", "b"]);
    }
}
