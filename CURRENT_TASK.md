# Current Task — Tessellation & boolean-group caching

Multi-phase work on two caches that share invalidation infrastructure.
Designed and agreed with Bart before starting; committing at each phase
boundary.

## Why

1. **Per-path tessellation cache** — the renderer currently re-tessellates
   the *entire* scene every frame that `Renderer.dirty` is set. Editing a
   single path's vertex causes every other path to re-tess. On scenes with
   thousands of paths (raster-traced SVGs, complex imports) this dominates
   frame time.

2. **Per-boolean-group computed-path cache** — `vector_bool::compute_boolean_group_path`
   runs on every frame inside the renderer's dirty-check tessellation loop,
   plus again from selection-bbox, scale-handle bounds, SVG export, and
   hit-testing. `i_overlay`'s polygon boolean is expensive (O(n log n) with
   a real constant) and we're currently paying it ~once per frame while
   a boolean group is visible.

3. They share invalidation: editing a path inside a boolean group must
   invalidate that path's tess *and* every ancestor boolean group's
   computed path. Building them separately would duplicate the
   invalidation logic.

## Design agreement

Captured from conversation with Bart.

### Q1: tess cache storage space — **local-space**

Tess output is cached in the path's local coordinates. The world transform
is applied per-draw (either one draw call per path with a transform
uniform, or a packed transforms buffer indexed by path_id with vertices
carrying the path_id).

Rationale: transform-only edits (drag-to-move, rotate, scale) are common
and must not bust the tess cache. GPU work is cheap, CPU tess is not.

### Q2: `Renderer.dirty` scene-wide flag — **keep**

Acts as a fast-path "nothing changed, skip the cache-check walk
entirely". Complements per-node caches rather than replacing them. If it
becomes a problem we can remove it later.

### Q3: invalidation API shape — **no manual bumps**

All mutations go through `Scene` methods that bump revisions
automatically. Manual `scene.bump(id)` sprinkled at call sites would be
silently forgotten — compiler-enforced is the only sustainable option.

Concretely: remove `pub fn get_mut` from `Scene`, replace with typed
setters. Add an `edit()` guard as the fallback for multi-field mutations.

## Revision model

Two revisions per node, stored in a `SecondaryMap<NodeId, NodeRevision>`
on `Scene` (SlotMap auto-drops on node removal):

```rust
pub struct NodeRevision {
    /// Bumps on Path / Style / GroupKind / TextData mutation.
    /// Used by the per-path tess cache.
    geometry_rev: u64,
    /// Bumps when self OR any descendant changes in ANY way
    /// (including transforms). Used by per-boolean-group path cache.
    subtree_rev: u64,
}
```

Why two and not one:

- Tess cache must survive transform-only edits (vertices don't move in
  local space).
- Boolean-group cache *must* invalidate on descendant transform edits
  (operands compose in the group's local space; moving a child changes
  the result).

Therefore:

| Mutation        | `geometry_rev` of self | `subtree_rev` of self + ancestors |
|-----------------|------------------------|-----------------------------------|
| set_path_data   | bump                   | bump                              |
| set_style       | bump                   | bump                              |
| set_group_kind  | bump                   | bump                              |
| set_text_data   | bump                   | bump                              |
| set_transform   | *no bump*              | bump                              |
| set_visible     | no bump                | no bump (caches don't care)       |
| set_locked      | no bump                | no bump                           |
| set_label       | no bump                | no bump                           |
| insert(parent)  | —                      | bump parent chain                 |
| remove(id)      | —                      | bump old parent chain             |
| reparent        | —                      | bump both old + new parent chains |

Ancestor walk uses existing `Scene::parent(id)` in a loop.

## Mutation API

```rust
impl Scene {
    // Typed setters — bump the right rev automatically.
    pub fn set_transform(&mut self, id: NodeId, t: Affine);
    pub fn set_path_data(&mut self, id: NodeId, p: Path);
    pub fn set_style(&mut self, id: NodeId, s: Style);
    pub fn set_group_kind(&mut self, id: NodeId, k: GroupKind);
    pub fn set_text_data(&mut self, id: NodeId, t: TextData);
    pub fn set_visible(&mut self, id: NodeId, v: bool);   // no bump
    pub fn set_locked(&mut self, id: NodeId, v: bool);    // no bump
    pub fn set_label(&mut self, id: NodeId, l: String);   // no bump

    /// Escape hatch — bumps both revs on drop (conservative).
    pub fn edit(&mut self, id: NodeId) -> Option<NodeEditGuard<'_>>;

    // Read-only access — unchanged.
    pub fn get(&self, id: NodeId) -> Option<&Node>;

    // Revision accessors for cache consumers.
    pub fn geometry_revision(&self, id: NodeId) -> u64;
    pub fn subtree_revision(&self, id: NodeId) -> u64;
}

pub struct NodeEditGuard<'a> { ... }  // DerefMut<Target = Node>; bumps on Drop
```

`pub fn get_mut` is **removed**. The compile-error fallout from this
removal is the whole point: every current mutation site surfaces and
gets converted to the right setter or to `edit()`.

## Phased delivery

### Phase A — Revision infrastructure *(no consumers yet)*

Pure refactor + new infrastructure. Mergeable as its own commit.

**Scope:**
1. Add `NodeRevision`, `revisions: SecondaryMap<NodeId, NodeRevision>`
   on `Scene`. Initialize to `(0, 0)` when a node is inserted.
2. Add internal `Scene::bump_geometry(id)` and `Scene::bump_subtree(id)`
   helpers; both walk `parent()` chain for subtree cascade.
3. Add typed setters (list above). Each calls the right internal bump.
4. Add `edit()` returning `NodeEditGuard` that bumps both on drop.
5. Remove `pub fn get_mut`. Audit every call site the compiler surfaces;
   convert to setter or guard.
6. Wire `insert` / `insert_at` / `insert_subtree` / `remove` / `reparent`
   to bump the affected parent chains.
7. Unit tests (in `vector-scene`):
   - `set_path_data` bumps geometry_rev + subtree_rev chain.
   - `set_transform` bumps subtree_rev only (not geometry_rev).
   - `insert` bumps parent's subtree_rev + all further ancestors.
   - `remove` bumps old parent chain.
   - `reparent` bumps both chains.
   - `edit()` guard bumps on drop.
   - Inserted node starts at (0, 0).

**Deliverable:** invalidation signal exists and is correct. Nothing
consumes it yet — the renderer and other sites keep their current
(naive, per-frame) behavior. Commit: `refactor(scene): per-node revision
tracking for cache invalidation`.

### Phase B — Boolean-group computed-path cache

Adds the first cache consumer. Small, self-contained in the renderer.

**Scope:**
1. `BoolPathCache: SecondaryMap<NodeId, CachedBoolPath>` field on
   `Renderer`, where `CachedBoolPath { subtree_rev: u64, path: Path }`.
2. Replace the three in-renderer `compute_boolean_group_path` call sites
   (main tess, selection bbox, scale-handle bounds) with a
   `cached_boolean_group_path(scene, id)` helper that checks the cache
   and recomputes only on subtree_rev mismatch.
3. Standalone call sites (`vector-svg`, `vector-tools`, flatten action,
   `vector-bool::boolean_group_visual_bounds`) stay uncached for now —
   they're one-shot, not on the hot loop. Revisit later if profiling
   says otherwise.
4. Test: unit test in vector-render or via a smoke test — touch a
   descendant's subtree_rev → cache miss → recompute; no touch → cache
   hit.
5. Cache eviction on node deletion is automatic (SecondaryMap).

**Deliverable:** zero i_overlay calls per frame during steady-state when
a boolean group is visible but idle. Commit: `perf(render): cache
boolean-group computed paths keyed on subtree revision`.

### Phase C — Per-path tess cache + local-space vertex pipeline

The bigger one. Touches the renderer's vertex/index buffer layout, the
shader, and the draw-call shape.

**Scope:**
1. `TessCache: SecondaryMap<NodeId, CachedMesh>` on `Renderer`, where
   `CachedMesh { geometry_rev: u64, vertices: Vec<Vertex>, indices: Vec<u32> }`.
2. Tessellation moves to local space (no `world_transform` applied at
   tess time).
3. Per-path `path_id` becomes a vertex attribute. Add a
   `TransformsBuffer` storage buffer (or uniform array) indexed by
   `path_id` carrying `mat4` world transforms. Vertex shader computes
   `out_pos = transforms[path_id] * vec4(in_pos, 0, 1)` then applies
   view-projection.
4. `Renderer::prepare()`:
   - Walk scene in draw order, collect `(NodeId, world_transform, path_id)`.
   - For each path node: cache hit → reuse cached vertices/indices with
     new `path_id`; cache miss → tessellate, store in cache.
   - For each boolean group: use Phase B cache, same handling.
   - After walk, assemble the combined vertex buffer with rewritten
     `path_id` attributes. Upload transforms buffer. Upload vertex buffer
     only if any cache miss happened.
5. Handle text nodes the same way (tess once, cache in local space).
6. The pattern paint texture pipeline is independent — leave alone.
7. Handle overlay (selection outlines, vertex handles, grid) stays as
   today — they're small and per-selection, not per-path.
8. Test: script-level — create a scene with N paths, modify one path's
   transform, verify tess_cache hit-rate = N-1 (or add a
   `tess_cache_stats()` debug accessor).

**Deliverable:** transform dragging on heavy scenes becomes free; path
edits only re-tess the edited path. Commit: `perf(render): per-path
tess cache with local-space vertices + per-draw transforms`.

## Risks / things to check during implementation

1. **`get_mut` call sites** — there are many, sprinkled across all tools.
   Phase A's refactor will surface them all at once. Budget time to fix
   cleanly rather than rushing.
2. **In-place vertex dragging during drag** — today this probably mutates
   through `get_mut`. Each drag-move tick will now bump geometry_rev and
   bust the tess cache for that path. That's correct — the path is
   actually changing — but worth noting we can't cache across drag
   frames. (Acceptable: a dragging path is tiny compared to a scene.)
3. **Tess output layout** — Phase C assumes vertices are per-path
   independent. Verify the current tessellator doesn't produce
   scene-global vertex references.
4. **Pattern & gradient refs** — cached tess vertices may encode paint
   references by ID. Verify those IDs are stable across frames; if not,
   cache needs an additional key dim.
5. **Boolean-group cache inside boolean groups** — `compute_boolean_group_path`
   recurses into nested boolean descendants. The cache should benefit
   nested groups too (each level caches its own path). Verify the
   recursive call in `vector_bool::lib.rs:281` goes through the cache,
   not the uncached function.

## Where we are now

- **Phase A done** — per-node revision tracking in `Scene`, `get_mut`
  removed, all mutation routed through typed setters / `edit()` guard /
  closure helpers. 17 new scene tests pass.
  Commit: `3510833 refactor(scene): per-node revision tracking for cache
  invalidation`.
- **Phase B done** — `BoolPathCache` in `vector-render`, keyed on
  `Scene::subtree_revision`. Replaces all three in-renderer
  `compute_boolean_group_path` call sites. 4 new cache tests pass.
  (Note on risk #5: nested-boolean recursion in
  `vector_bool::compute_boolean_group_path` still goes through the
  uncached function, but in the common steady state the outer cache
  short-circuits before recursion happens at all, so nested groups
  effectively benefit. Filed as nice-to-have in TODO.md.)
- **Phase C done** — `TessCache` in `vector-render`, keyed on each
  node's `Scene::geometry_revision`. Tessellation moved into local
  space; each vertex carries a `path_id` that indexes into a new
  `transforms` storage buffer populated per frame. Transform-only
  edits (drag, rotate, scale, properties-panel X/Y) no longer re-tess
  any path — the tess cache is reused verbatim and only the transforms
  buffer is re-uploaded. Boolean groups intentionally skip this cache
  (their `geometry_rev`/`subtree_rev` combo over-invalidates on
  group-own transforms, and re-tessellating the Phase-B-cached
  computed path is cheap anyway). 4 new tess_cache tests pass,
  including the N-paths-1-transform-edit → N hits check from the plan.
