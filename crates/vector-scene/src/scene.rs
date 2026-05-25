use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use slotmap::{SecondaryMap, SlotMap};

use crate::node::{GroupKind, Node, NodeData, RasterImage, TextData};
use crate::paint::{Gradient, Paint, PaintRef};
use crate::style::Style;
use vector_geom::{Affine, Bounds, Path, Point};

// SlotMap's DefaultKey gives us stable, generational IDs for free:
// - O(1) lookup
// - IDs survive insertions/deletions of other nodes
// - Stale IDs (pointing at deleted nodes) are detected automatically
slotmap::new_key_type! {
    /// Stable identifier for a node in the scene graph.
    pub struct NodeId;
}

/// Per-node revision counters, used for cache invalidation.
///
/// Two counters are tracked:
/// * `geometry_rev` — bumps when the node's own rendered geometry changes
///   (Path data, Style, GroupKind's Boolean style, TextData). Drives the
///   per-path tessellation cache: if `geometry_rev` is unchanged since
///   last tess, the cached mesh is still valid in local space.
/// * `subtree_rev` — bumps whenever *any* descendant (including self) in
///   the node's subtree changes in any way (geometry, transform, structural
///   insert/remove/reparent). Drives the per-boolean-group computed-path
///   cache: a boolean group's baked `Path` depends on every descendant's
///   geometry *and* transform (operands compose in the group's local
///   space), so any subtree change invalidates it.
///
/// Transform edits bump `subtree_rev` (of self + ancestors) but NOT
/// `geometry_rev` — local-space tessellation output is unchanged when
/// a node's transform changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeRevision {
    pub geometry_rev: u64,
    pub subtree_rev: u64,
}

/// The scene: a flat pool of nodes with a designated root.
/// Parent-child relationships are stored inside each node (children vec),
/// with an auxiliary parent lookup for walking up the tree.
///
/// Mutation invariant: the only way to obtain a mutable reference to a
/// `Node` is via one of the typed `set_*` methods or the `edit()` guard.
/// Both are wired to bump the right revision counters. There is no
/// `pub fn get_mut` — this is deliberate, so that cache invalidation
/// cannot be silently skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    nodes: SlotMap<NodeId, Node>,
    /// Parent lookup: child -> parent. Root has no entry.
    parents: SecondaryMap<NodeId, NodeId>,
    /// Per-node revision counters for cache invalidation.
    revisions: SecondaryMap<NodeId, NodeRevision>,
    /// Scene-wide UI revision. Bumps on *any* mutation, including those
    /// that don't affect the renderer's geometry caches (visibility,
    /// locked, label, structure). Drives UI-side caches (e.g. the
    /// structure panel's flattened-tree buffer) that must reflect the
    /// full state shown to the user, not just rendered geometry.
    #[serde(default)]
    ui_rev: u64,
    /// The root group node. Always exists.
    root: NodeId,
    /// The defs group (non-rendered). Child of root, always exists.
    defs: NodeId,
    /// Document viewBox — the rectangle in canvas coordinates that represents
    /// the "page". Stored as document metadata so SVG round-trips preserve
    /// what the user set; the renderer draws a page rectangle here so the
    /// user can see where content will be framed on export. Not derived from
    /// content bounds — that's an explicit user action ("Fit Page to Content").
    #[serde(default = "default_view_box")]
    view_box: Bounds,
}

fn default_view_box() -> Bounds {
    Bounds::new(Point::new(0.0, 0.0), Point::new(800.0, 600.0))
}

impl Scene {
    /// Create a new empty scene with a root group and a defs group.
    pub fn new() -> Self {
        let mut nodes = SlotMap::with_key();
        let mut parents = SecondaryMap::new();
        let mut revisions = SecondaryMap::new();

        let root = nodes.insert(Node::group("root"));
        revisions.insert(root, NodeRevision::default());

        let defs = nodes.insert(Node {
            label: "defs".into(),
            data: NodeData::Group {
                is_defs: true,
                kind: GroupKind::Regular,
            },
            ..Node::group("")
        });
        revisions.insert(defs, NodeRevision::default());

        // defs is a child of root
        nodes[root].children.push(defs);
        parents.insert(defs, root);

        Self {
            nodes,
            parents,
            revisions,
            ui_rev: 0,
            root,
            defs,
            view_box: default_view_box(),
        }
    }

    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn defs(&self) -> NodeId {
        self.defs
    }

    /// The document viewBox (page rectangle in canvas coordinates).
    pub fn view_box(&self) -> Bounds {
        self.view_box
    }

    /// Replace the document viewBox. Bumps the UI revision so the renderer's
    /// page-rect overlay redraws and panels that show document metadata can
    /// refresh.
    pub fn set_view_box(&mut self, bounds: Bounds) {
        if self.view_box != bounds {
            self.view_box = bounds;
            self.ui_rev = self.ui_rev.wrapping_add(1);
        }
    }

    /// Get a node by ID.
    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Get the parent of a node.
    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        self.parents.get(id).copied()
    }

    /// Whether `id` is `ancestor` itself or a transitive descendant of it.
    /// Walks the parent chain — bounded by tree depth, which is small in
    /// practice. Useful for "is this node inside that subtree" checks
    /// without materializing the descendant set.
    pub fn is_descendant_or_self(&self, id: NodeId, ancestor: NodeId) -> bool {
        if id == ancestor {
            return true;
        }
        let mut cur = id;
        while let Some(parent) = self.parent(cur) {
            if parent == ancestor {
                return true;
            }
            cur = parent;
        }
        false
    }

    // ── Revision accessors ─────────────────────────────────────────

    /// Current geometry revision for `id` (0 if missing).
    ///
    /// Bumps when Path / Style / GroupKind / TextData mutate on this
    /// specific node. Does NOT bump on transform changes. Cache consumers
    /// that operate on local-space geometry (e.g. tessellation output)
    /// key on this value.
    pub fn geometry_revision(&self, id: NodeId) -> u64 {
        self.revisions.get(id).map_or(0, |r| r.geometry_rev)
    }

    /// Current subtree revision for `id` (0 if missing).
    ///
    /// Bumps whenever this node OR any descendant changes in any way,
    /// including transforms and structural edits. Cache consumers whose
    /// output depends on the whole subtree (e.g. a boolean group's baked
    /// path, which composes descendant geometry in the group's local
    /// space) key on this value.
    pub fn subtree_revision(&self, id: NodeId) -> u64 {
        self.revisions.get(id).map_or(0, |r| r.subtree_rev)
    }

    /// Scene-wide UI revision. Bumps on every mutation routed through
    /// `Scene` (typed setters, `edit()` guard drop, structural ops,
    /// visibility/locked/label flips). Stable while idle. Cheap to
    /// compare — it's a single `u64` on `Scene`.
    pub fn ui_revision(&self) -> u64 {
        self.ui_rev
    }

    // ── Typed setters ──────────────────────────────────────────────
    //
    // Every mutation of a `Node`'s field goes through one of these
    // methods (or the `edit()` guard). Each bumps exactly the revisions
    // it needs to and no more.

    /// Replace a node's local transform, returning the previous value.
    /// Bumps `subtree_rev` of self + ancestors. Does NOT bump
    /// `geometry_rev` (local tessellation is unaffected by transform).
    /// Returns `None` if the node doesn't exist.
    pub fn set_transform(&mut self, id: NodeId, transform: Affine) -> Option<Affine> {
        let node = self.nodes.get_mut(id)?;
        let old = std::mem::replace(&mut node.transform, transform);
        self.bump_subtree_chain(id);
        Some(old)
    }

    /// Replace a path node's `Path` data, returning the previous value.
    /// Bumps `geometry_rev` of self and `subtree_rev` of self + ancestors.
    /// Returns `None` if the node doesn't exist or isn't a `Path`.
    pub fn set_path_data(&mut self, id: NodeId, path: Path) -> Option<Path> {
        let node = self.nodes.get_mut(id)?;
        let NodeData::Path { path: current, .. } = &mut node.data else {
            return None;
        };
        let old = std::mem::replace(current, path);
        self.bump_geometry(id);
        Some(old)
    }

    /// Replace a path or boolean-group's `Style`, returning the previous value.
    /// Bumps `geometry_rev` of self and `subtree_rev` of self + ancestors.
    /// Returns `None` if the node doesn't exist or isn't a `Path`/boolean `Group`.
    pub fn set_style(&mut self, id: NodeId, style: Style) -> Option<Style> {
        let node = self.nodes.get_mut(id)?;
        let current = match &mut node.data {
            NodeData::Path { style: current, .. } => current,
            NodeData::Group {
                kind: GroupKind::Boolean { style: current, .. },
                ..
            } => current,
            _ => return None,
        };
        let old = std::mem::replace(current, style);
        self.bump_geometry(id);
        Some(old)
    }

    /// Replace a group node's `GroupKind`, returning the previous value.
    /// Bumps `geometry_rev` of self and `subtree_rev` of self + ancestors
    /// (switching between Regular and Boolean changes what the group
    /// renders as, even if descendants didn't change).
    /// Returns `None` if the node doesn't exist or isn't a `Group`.
    pub fn set_group_kind(&mut self, id: NodeId, kind: GroupKind) -> Option<GroupKind> {
        let node = self.nodes.get_mut(id)?;
        let NodeData::Group { kind: current, .. } = &mut node.data else {
            return None;
        };
        let old = std::mem::replace(current, kind);
        self.bump_geometry(id);
        Some(old)
    }

    /// Replace the `Gradient` carried by a `NodeData::Paint(Paint::Gradient)`
    /// node, returning the previous value. Bumps `geometry_rev` of self
    /// (paths referencing this gradient also need their tessellation cache
    /// invalidated, which they handle by also keying on this node's
    /// `geometry_rev`) and `subtree_rev` of self + ancestors.
    /// Returns `None` if the node doesn't exist or isn't a gradient paint.
    ///
    /// NOTE: paths referencing this gradient via `PaintRef::Ref` won't have
    /// their own `geometry_rev` bumped — they don't need to re-tessellate;
    /// only the gradient uniform on the GPU side needs to be re-uploaded.
    /// The renderer already invalidates per-path caches via the gradient
    /// node's revision.
    pub fn set_gradient(&mut self, id: NodeId, gradient: Gradient) -> Option<Gradient> {
        let node = self.nodes.get_mut(id)?;
        let NodeData::Paint(Paint::Gradient(current)) = &mut node.data else {
            return None;
        };
        let old = std::mem::replace(current, gradient);
        self.bump_geometry(id);
        Some(old)
    }

    /// Replace a text node's `TextData`, returning the previous value.
    /// Bumps `geometry_rev` of self and `subtree_rev` of self + ancestors.
    /// Returns `None` if the node doesn't exist or isn't a `Text`.
    pub fn set_text_data(&mut self, id: NodeId, text: TextData) -> Option<TextData> {
        let node = self.nodes.get_mut(id)?;
        let NodeData::Text(current) = &mut node.data else {
            return None;
        };
        let old = std::mem::replace(current, text);
        self.bump_geometry(id);
        Some(old)
    }

    /// Set a node's `visible` flag. Does NOT bump any revision — visibility
    /// doesn't affect any cached geometry. Returns `false` if the node
    /// doesn't exist.
    pub fn set_visible(&mut self, id: NodeId, visible: bool) -> bool {
        match self.nodes.get_mut(id) {
            Some(n) => {
                n.visible = visible;
                self.ui_rev = self.ui_rev.wrapping_add(1);
                true
            }
            None => false,
        }
    }

    /// Set a node's `locked` flag. Does NOT bump any revision.
    /// Returns `false` if the node doesn't exist.
    pub fn set_locked(&mut self, id: NodeId, locked: bool) -> bool {
        match self.nodes.get_mut(id) {
            Some(n) => {
                n.locked = locked;
                self.ui_rev = self.ui_rev.wrapping_add(1);
                true
            }
            None => false,
        }
    }

    /// Set a node's human-readable label. Does NOT bump any revision.
    /// Returns `false` if the node doesn't exist.
    pub fn set_label(&mut self, id: NodeId, label: String) -> bool {
        match self.nodes.get_mut(id) {
            Some(n) => {
                n.label = label;
                self.ui_rev = self.ui_rev.wrapping_add(1);
                true
            }
            None => false,
        }
    }

    /// Run `f` with mutable access to a path node's `Path`, bumping
    /// `geometry_rev` on success. The closure returns an arbitrary value
    /// that's threaded through. Useful for partial / deep mutations into
    /// the path structure (pushing segments, editing vertices) that
    /// don't warrant a full `set_path_data` clone-and-swap.
    ///
    /// Returns `None` if the node is missing or isn't a `Path`. Does not
    /// bump if `None` is returned (nothing was mutated).
    pub fn with_path_data_mut<F, R>(&mut self, id: NodeId, f: F) -> Option<R>
    where
        F: FnOnce(&mut Path) -> R,
    {
        let node = self.nodes.get_mut(id)?;
        let NodeData::Path { path, .. } = &mut node.data else {
            return None;
        };
        let r = f(path);
        self.bump_geometry(id);
        Some(r)
    }

    /// Run `f` with mutable access to a text node's `TextData`, bumping
    /// `geometry_rev` on success. Analogous to `with_path_data_mut`.
    pub fn with_text_data_mut<F, R>(&mut self, id: NodeId, f: F) -> Option<R>
    where
        F: FnOnce(&mut TextData) -> R,
    {
        let node = self.nodes.get_mut(id)?;
        let NodeData::Text(text) = &mut node.data else {
            return None;
        };
        let r = f(text);
        self.bump_geometry(id);
        Some(r)
    }

    /// Acquire a mutation guard for a node. The guard derefs to `&mut Node`
    /// and, on drop, conservatively bumps both `geometry_rev` and
    /// `subtree_rev` (cascading subtree_rev up the ancestor chain) since
    /// it can't know what the caller changed.
    ///
    /// Prefer the typed setters when you're mutating a single field —
    /// they bump more narrowly. Use this guard for compound mutations
    /// (e.g. in-place vertex drag that walks into `Path.subpaths`) where
    /// typed setters would require a clone-and-swap.
    ///
    /// Returns `None` if the node doesn't exist.
    pub fn edit(&mut self, id: NodeId) -> Option<NodeEditGuard<'_>> {
        if !self.nodes.contains_key(id) {
            return None;
        }
        Some(NodeEditGuard { scene: self, id })
    }

    // ── Internal bump helpers (private) ────────────────────────────

    /// Bump `geometry_rev` of `id`, then cascade `subtree_rev` up from
    /// `id` to the root (inclusive).
    fn bump_geometry(&mut self, id: NodeId) {
        if let Some(rev) = self.revisions.get_mut(id) {
            rev.geometry_rev = rev.geometry_rev.wrapping_add(1);
        }
        self.bump_subtree_chain(id);
    }

    /// Cascade `subtree_rev` upward starting at `id` (inclusive), walking
    /// every ancestor via `parents`. Used by transform edits and by the
    /// tail of `bump_geometry`.
    fn bump_subtree_chain(&mut self, id: NodeId) {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if let Some(rev) = self.revisions.get_mut(c) {
                rev.subtree_rev = rev.subtree_rev.wrapping_add(1);
            }
            cur = self.parents.get(c).copied();
        }
        // Any structural / geometric / transform mutation also bumps the
        // scene-wide UI rev — that's the catch-all signal for UI-side
        // caches that don't care which specific node changed.
        self.ui_rev = self.ui_rev.wrapping_add(1);
    }

    // ── Structural operations ──────────────────────────────────────

    /// Insert a new node as a child of `parent` (appended at the end).
    /// Returns the new node's ID. Bumps `subtree_rev` of `parent` and all
    /// its ancestors (the subtree gained a new descendant).
    pub fn insert(&mut self, parent: NodeId, node: Node) -> Option<NodeId> {
        self.insert_at(parent, node, None)
    }

    /// Convenience: build and insert a raster image node as a child of
    /// `parent`. Returns the new `NodeId`. The local-space box runs from
    /// `(0, 0)` to `(width, height)`; place the box on the canvas by
    /// editing the new node's transform afterwards.
    pub fn add_raster(
        &mut self,
        parent: NodeId,
        label: impl Into<String>,
        image: Arc<RasterImage>,
        width: f64,
        height: f64,
    ) -> Option<NodeId> {
        self.insert(parent, Node::raster(label, image, width, height))
    }

    /// Insert a new node as a child of `parent` at a specific index.
    /// If `index` is `None`, the node is appended at the end.
    /// Bumps `subtree_rev` of `parent` and all its ancestors.
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
        self.revisions.insert(id, NodeRevision::default());
        let children = &mut self.nodes[parent].children;
        match index {
            Some(idx) => children.insert(idx.min(children.len()), id),
            None => children.push(id),
        }
        self.parents.insert(id, parent);
        self.bump_subtree_chain(parent);
        Some(id)
    }

    /// Returns the index of `id` within its parent's children list.
    pub fn child_index(&self, id: NodeId) -> Option<usize> {
        let parent_id = self.parents.get(id).copied()?;
        let parent = self.nodes.get(parent_id)?;
        parent.children.iter().position(|c| *c == id)
    }

    /// Remove a node and all its descendants. Returns the removed nodes.
    /// Bumps `subtree_rev` of the old parent and all its ancestors.
    pub fn remove(&mut self, id: NodeId) -> Vec<(NodeId, Node)> {
        if id == self.root || id == self.defs {
            return Vec::new(); // can't remove structural nodes
        }
        let old_parent = self.parents.get(id).copied();
        let mut removed = Vec::new();
        self.remove_recursive(id, &mut removed);

        // Remove from parent's children list
        if let Some(parent_id) = self.parents.remove(id)
            && let Some(parent) = self.nodes.get_mut(parent_id)
        {
            parent.children.retain(|c| *c != id);
        }

        // Cascade invalidation up from the old parent.
        if let Some(parent_id) = old_parent {
            self.bump_subtree_chain(parent_id);
        }
        removed
    }

    fn remove_recursive(&mut self, id: NodeId, out: &mut Vec<(NodeId, Node)>) {
        if let Some(node) = self.nodes.remove(id) {
            self.revisions.remove(id);
            let children: Vec<NodeId> = node.children.clone();
            out.push((id, node));
            for child in children {
                self.parents.remove(child);
                self.remove_recursive(child, out);
            }
        }
    }

    /// Move a node to be a child of a new parent at a given index.
    /// Bumps `subtree_rev` of both old and new parent chains.
    pub fn reparent(&mut self, id: NodeId, new_parent: NodeId, index: usize) -> bool {
        if id == self.root || id == self.defs || !self.nodes.contains_key(new_parent) {
            return false;
        }

        let old_parent = self.parents.get(id).copied();

        // Remove from old parent
        if let Some(op) = old_parent
            && let Some(parent) = self.nodes.get_mut(op)
        {
            parent.children.retain(|c| *c != id);
        }

        // Add to new parent
        let children = &mut self.nodes[new_parent].children;
        let idx = index.min(children.len());
        children.insert(idx, id);
        self.parents.insert(id, new_parent);

        // Cascade invalidation from both the old and new parent chains.
        // The moved subtree's own geometry/subtree revisions are
        // unchanged — its contents haven't changed, only its parent.
        if let Some(op) = old_parent {
            self.bump_subtree_chain(op);
        }
        self.bump_subtree_chain(new_parent);
        true
    }

    /// Iterate over all nodes.
    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes.iter()
    }

    /// Walk the tree depth-first from a given root, calling `f` for each node
    /// with its accumulated world transform.
    ///
    /// The visitor returns a `bool` indicating whether to descend into this
    /// node's children: `true` = walk the subtree, `false` = prune the
    /// subtree. This is how visibility cascades — if a group is hidden, its
    /// visitor returns `false` and the entire subtree is skipped, so hidden
    /// groups hide their children even though groups themselves render
    /// nothing.
    pub fn walk_depth_first(
        &self,
        from: NodeId,
        parent_transform: Affine,
        f: &mut impl FnMut(NodeId, &Node, Affine) -> bool,
    ) {
        let Some(node) = self.get(from) else {
            return;
        };
        let world = parent_transform.then(node.transform);
        if !f(from, node, world) {
            return;
        }
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
    pub fn world_transform(&self, id: NodeId) -> Affine {
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
            .map_or(Affine::IDENTITY, |n| n.transform);
        for &node_id in chain.iter().rev() {
            if let Some(node) = self.nodes.get(node_id) {
                xform = xform.then(node.transform);
            }
        }
        xform
    }

    /// True iff `id` and every one of its ancestors has `visible == true`.
    /// An invisible parent hides all of its descendants for rendering /
    /// hit-testing purposes — this is the canonical check for "should
    /// anything related to this node show on the canvas?".
    pub fn is_visible_in_world(&self, id: NodeId) -> bool {
        let mut current = id;
        loop {
            let Some(node) = self.nodes.get(current) else {
                return false;
            };
            if !node.visible {
                return false;
            }
            match self.parents.get(current).copied() {
                Some(parent) => current = parent,
                None => return true,
            }
        }
    }

    /// Compute the accumulated world transform of a node's *parent chain*,
    /// excluding the node's own transform. Useful for converting a world-space
    /// delta into the local space where the node's transform operates.
    pub fn parent_world_transform(&self, id: NodeId) -> Affine {
        let parent_id = match self.parents.get(id) {
            Some(&p) => p,
            None => return Affine::IDENTITY,
        };
        self.world_transform(parent_id)
    }

    /// Count how many style fields (fills + strokes across all path / text /
    /// boolean-group nodes) reference `paint_node` via `PaintRef::Ref`.
    /// Returns 0 if no node references it (or if the node doesn't exist).
    ///
    /// Used by the paint editor to decide whether editing a gradient
    /// affects multiple shapes — a count > 1 surfaces a "Make unique"
    /// affordance so the user can fork the gradient before editing.
    ///
    /// A single fill+stroke both pointing at the same gradient counts as
    /// 2; that's deliberate. From the gradient's perspective, that's two
    /// independent references; the user should be informed.
    ///
    // TODO(orphaned-defs): When a paint-type switch in the editor changes
    // a style's `paint` from `PaintRef::Ref(g)` to anything else, the
    // gradient/pattern node `g` may become unreferenced — but we don't
    // currently delete it. A future GC pass should walk the defs subtree,
    // call `gradient_ref_count` (and the equivalent for patterns) for
    // each entry, and prune zero-ref nodes. This must be a periodic /
    // explicit pass rather than ref-count-on-mutation: cyclic refs and
    // multi-mutation transactions would otherwise drop a node that
    // becomes referenced again two operations later.
    pub fn gradient_ref_count(&self, paint_node: NodeId) -> usize {
        let mut count = 0;
        for (_, node) in self.nodes.iter() {
            node.for_each_paint_ref(|paint| {
                if matches!(paint, PaintRef::Ref(id) if *id == paint_node) {
                    count += 1;
                }
            });
        }
        count
    }

    /// Deep-clone a `NodeData::Paint` node into the same parent (typically
    /// `defs`) and return the new node's id. The returned id can then be
    /// stored in a `PaintRef::Ref` to "fork" the paint — used by the
    /// "Make unique" affordance in the paint editor.
    ///
    /// Returns `None` if `src` doesn't exist, isn't a `Paint` node, or has
    /// no parent (i.e. is the root).
    pub fn clone_paint_node(&mut self, src: NodeId) -> Option<NodeId> {
        let node = self.nodes.get(src)?;
        if !matches!(node.data, NodeData::Paint(_)) {
            return None;
        }
        let parent = self.parent(src)?;
        let mut clone = node.clone();
        clone.children.clear(); // paint nodes shouldn't have children, but be defensive
        self.insert(parent, clone)
    }

    /// Compute the bounding box of all visible content (paths and text),
    /// skipping defs, taking group transforms into account. Includes the
    /// visible stroke area around each shape.
    ///
    /// This is the naive variant — it treats every node's `visual_bounds`
    /// independently. For scenes with Boolean groups, prefer
    /// `vector_bool::scene_content_bounds`, which short-circuits at
    /// Boolean groups and returns their tight baked-path bounds instead
    /// of the looser union-of-operands bounds.
    pub fn content_bounds(&self) -> vector_geom::Bounds {
        let mut bounds = vector_geom::Bounds::EMPTY;
        self.walk_depth_first(self.root, Affine::IDENTITY, &mut |_id, node, world| {
            if !node.visible {
                return false;
            }
            let node_bounds = node.data.visual_bounds(world);
            if !node_bounds.is_empty() {
                bounds = bounds.union(node_bounds);
            }
            true
        });
        bounds
    }
}

// ── Mutation guard ────────────────────────────────────────────────────

/// RAII guard returned by `Scene::edit`. Derefs to `&mut Node` for
/// compound mutations (e.g. in-place path vertex edits). On drop, bumps
/// both `geometry_rev` of the target node and `subtree_rev` of self +
/// ancestors — conservatively, since the guard can't observe what was
/// actually changed.
///
/// Prefer typed setters (`set_transform`, `set_path_data`, etc.) when
/// possible; they bump more narrowly.
pub struct NodeEditGuard<'a> {
    scene: &'a mut Scene,
    id: NodeId,
}

impl<'a> NodeEditGuard<'a> {
    /// The ID of the node being edited.
    pub fn id(&self) -> NodeId {
        self.id
    }
}

impl<'a> Deref for NodeEditGuard<'a> {
    type Target = Node;
    fn deref(&self) -> &Node {
        // The guard is only created when the node exists; SlotMap keys
        // are generational, so indexing is safe here.
        &self.scene.nodes[self.id]
    }
}

impl<'a> DerefMut for NodeEditGuard<'a> {
    fn deref_mut(&mut self) -> &mut Node {
        &mut self.scene.nodes[self.id]
    }
}

impl<'a> Drop for NodeEditGuard<'a> {
    fn drop(&mut self) {
        self.scene.bump_geometry(self.id);
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
    use vector_geom::{Point, Segment, SubPath, VertexMode};

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

    // ── Revision tracking tests ───────────────────────────────────

    #[test]
    fn new_node_starts_at_zero_revisions() {
        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", make_test_path()))
            .unwrap();
        assert_eq!(scene.geometry_revision(id), 0);
        assert_eq!(scene.subtree_revision(id), 0);
    }

    #[test]
    fn set_path_data_bumps_geometry_and_subtree() {
        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", make_test_path()))
            .unwrap();
        let geom0 = scene.geometry_revision(id);
        let sub0 = scene.subtree_revision(id);

        let new_path = make_test_path();
        assert!(scene.set_path_data(id, new_path).is_some());

        assert!(scene.geometry_revision(id) > geom0, "geometry should bump");
        assert!(scene.subtree_revision(id) > sub0, "subtree should bump");
    }

    #[test]
    fn set_transform_bumps_subtree_only_not_geometry() {
        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", make_test_path()))
            .unwrap();
        let geom0 = scene.geometry_revision(id);
        let sub0 = scene.subtree_revision(id);

        let t = Affine {
            a: 2.0,
            b: 0.0,
            c: 0.0,
            d: 2.0,
            tx: 5.0,
            ty: 7.0,
        };
        assert!(scene.set_transform(id, t).is_some());

        assert_eq!(
            scene.geometry_revision(id),
            geom0,
            "geometry must NOT bump on transform change"
        );
        assert!(
            scene.subtree_revision(id) > sub0,
            "subtree should bump on transform change"
        );
    }

    #[test]
    fn geometry_edit_cascades_subtree_rev_to_ancestors() {
        let mut scene = Scene::new();
        let root = scene.root();
        let outer = scene.insert(root, Node::group("outer")).unwrap();
        let inner = scene.insert(outer, Node::group("inner")).unwrap();
        let leaf = scene
            .insert(inner, Node::path("p", make_test_path()))
            .unwrap();

        // Record all ancestor subtree_revs after the chain was built.
        let root_s0 = scene.subtree_revision(root);
        let outer_s0 = scene.subtree_revision(outer);
        let inner_s0 = scene.subtree_revision(inner);
        let leaf_s0 = scene.subtree_revision(leaf);

        // Edit the leaf.
        scene.set_path_data(leaf, make_test_path()).unwrap();

        // Every node on the chain should have bumped subtree_rev.
        assert!(
            scene.subtree_revision(leaf) > leaf_s0,
            "leaf subtree should bump"
        );
        assert!(
            scene.subtree_revision(inner) > inner_s0,
            "inner subtree should bump"
        );
        assert!(
            scene.subtree_revision(outer) > outer_s0,
            "outer subtree should bump"
        );
        assert!(
            scene.subtree_revision(root) > root_s0,
            "root subtree should bump"
        );
    }

    #[test]
    fn insert_bumps_parent_chain_subtree() {
        let mut scene = Scene::new();
        let root = scene.root();
        let outer = scene.insert(root, Node::group("outer")).unwrap();
        let outer_s0 = scene.subtree_revision(outer);
        let root_s0 = scene.subtree_revision(root);

        // Insert a leaf inside outer.
        scene
            .insert(outer, Node::path("p", make_test_path()))
            .unwrap();

        assert!(
            scene.subtree_revision(outer) > outer_s0,
            "outer subtree should bump on insert"
        );
        assert!(
            scene.subtree_revision(root) > root_s0,
            "root subtree should bump on insert"
        );
    }

    #[test]
    fn remove_bumps_old_parent_chain_subtree() {
        let mut scene = Scene::new();
        let root = scene.root();
        let outer = scene.insert(root, Node::group("outer")).unwrap();
        let leaf = scene
            .insert(outer, Node::path("p", make_test_path()))
            .unwrap();
        let outer_s0 = scene.subtree_revision(outer);
        let root_s0 = scene.subtree_revision(root);

        scene.remove(leaf);

        assert!(scene.subtree_revision(outer) > outer_s0);
        assert!(scene.subtree_revision(root) > root_s0);
    }

    #[test]
    fn reparent_bumps_both_parent_chains() {
        let mut scene = Scene::new();
        let root = scene.root();
        let a = scene.insert(root, Node::group("a")).unwrap();
        let b = scene.insert(root, Node::group("b")).unwrap();
        let leaf = scene.insert(a, Node::path("p", make_test_path())).unwrap();

        let a_s0 = scene.subtree_revision(a);
        let b_s0 = scene.subtree_revision(b);

        assert!(scene.reparent(leaf, b, 0));

        assert!(
            scene.subtree_revision(a) > a_s0,
            "old parent subtree should bump"
        );
        assert!(
            scene.subtree_revision(b) > b_s0,
            "new parent subtree should bump"
        );
    }

    #[test]
    fn edit_guard_bumps_on_drop() {
        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", make_test_path()))
            .unwrap();
        let geom0 = scene.geometry_revision(id);
        let sub0 = scene.subtree_revision(id);

        {
            let mut guard = scene.edit(id).unwrap();
            guard.label = "renamed".into();
            // Even though we only changed a meta field, the guard bumps
            // conservatively on drop — that's the documented trade-off.
        }

        assert!(scene.geometry_revision(id) > geom0);
        assert!(scene.subtree_revision(id) > sub0);
    }

    #[test]
    fn set_visible_does_not_bump() {
        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", make_test_path()))
            .unwrap();
        let geom0 = scene.geometry_revision(id);
        let sub0 = scene.subtree_revision(id);

        assert!(scene.set_visible(id, false));

        assert_eq!(scene.geometry_revision(id), geom0);
        assert_eq!(scene.subtree_revision(id), sub0);
    }

    #[test]
    fn ui_revision_bumps_on_every_mutation() {
        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", make_test_path()))
            .unwrap();

        // Geometric / transform / structural edits all bump.
        let r0 = scene.ui_revision();
        scene.set_transform(id, Affine::translate(1.0, 2.0));
        let r1 = scene.ui_revision();
        assert!(r1 > r0, "transform should bump ui_rev");

        scene.set_path_data(id, make_test_path());
        let r2 = scene.ui_revision();
        assert!(r2 > r1, "path data should bump ui_rev");

        // The "no-bump" setters (visible/locked/label) bump ui_rev
        // because the structure panel cache does care about them.
        assert!(scene.set_visible(id, false));
        let r3 = scene.ui_revision();
        assert!(r3 > r2, "set_visible should bump ui_rev");

        assert!(scene.set_locked(id, true));
        let r4 = scene.ui_revision();
        assert!(r4 > r3, "set_locked should bump ui_rev");

        assert!(scene.set_label(id, "renamed".into()));
        let r5 = scene.ui_revision();
        assert!(r5 > r4, "set_label should bump ui_rev");

        // Structural insert bumps.
        let _id2 = scene
            .insert(scene.root(), Node::path("q", make_test_path()))
            .unwrap();
        let r6 = scene.ui_revision();
        assert!(r6 > r5, "insert should bump ui_rev");

        // edit() guard bumps on drop.
        {
            let mut g = scene.edit(id).unwrap();
            g.label = "again".into();
        }
        let r7 = scene.ui_revision();
        assert!(r7 > r6, "edit guard should bump ui_rev on drop");
    }
}
