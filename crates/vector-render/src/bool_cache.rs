//! Per-boolean-group computed-path cache.
//!
//! Boolean groups (`GroupKind::Boolean { op, .. }`) render their content as
//! a single computed `Path` — the polygon boolean of the subtree's operand
//! paths. Computing that path is expensive (runs `i_overlay` on every
//! descendant), so we cache the result per group and recompute only when
//! the group's `subtree_rev` changes.
//!
//! The `subtree_rev` covers *every* kind of subtree change:
//!   - geometry edits to descendant paths,
//!   - transform edits on descendants (operands compose in the group's
//!     local space, so a moved child changes the result),
//!   - structural edits (insert/remove/reparent descendants),
//!
//! so keying on it is both necessary and sufficient.
//!
//! Backed by a `SecondaryMap`, so cache entries are dropped automatically
//! when their node is removed from the scene.

use slotmap::SecondaryMap;
use vector_geom::Path;
use vector_scene::{NodeId, Scene};

struct CachedBoolPath {
    subtree_rev: u64,
    path: Path,
}

/// Caches `vector_bool::compute_boolean_group_path` results per boolean
/// group, keyed on the group's `Scene::subtree_revision`.
#[derive(Default)]
pub(crate) struct BoolPathCache {
    entries: SecondaryMap<NodeId, CachedBoolPath>,
}

impl BoolPathCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached computed path for `group_id`, recomputing it if
    /// the group's `subtree_rev` has advanced since the last call (or the
    /// entry is fresh).
    pub fn get_or_compute(&mut self, scene: &Scene, group_id: NodeId) -> &Path {
        let rev = scene.subtree_revision(group_id);
        let stale = self
            .entries
            .get(group_id)
            .is_none_or(|c| c.subtree_rev != rev);
        if stale {
            let path = vector_bool::compute_boolean_group_path(scene, group_id);
            self.entries.insert(
                group_id,
                CachedBoolPath {
                    subtree_rev: rev,
                    path,
                },
            );
        }
        &self.entries[group_id].path
    }

    /// Number of cached entries (for tests / debug HUD).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_geom::{Path, Point, Segment, SubPath, VertexMode};
    use vector_scene::{BoolOp, GroupKind, Node, Scene, Style};

    fn square(x: f64, y: f64, size: f64) -> Path {
        let mut p = Path::new();
        p.subpaths.push(SubPath {
            start: Point::new(x, y),
            segments: vec![
                Segment::Line {
                    to: Point::new(x + size, y),
                },
                Segment::Line {
                    to: Point::new(x + size, y + size),
                },
                Segment::Line {
                    to: Point::new(x, y + size),
                },
            ],
            closed: true,
            vertex_modes: vec![VertexMode::Corner; 4],
        });
        p
    }

    fn setup() -> (Scene, NodeId, NodeId) {
        let mut scene = Scene::new();
        let root = scene.root();
        let group = scene.insert(root, Node::group("bool")).expect("group");
        scene
            .set_group_kind(
                group,
                GroupKind::Boolean {
                    op: BoolOp::Union,
                    style: Style::default(),
                },
            )
            .expect("set boolean");
        let a = scene
            .insert(group, Node::path("a", square(0.0, 0.0, 10.0)))
            .expect("a");
        scene
            .insert(group, Node::path("b", square(5.0, 0.0, 10.0)))
            .expect("b");
        (scene, group, a)
    }

    #[test]
    fn first_call_computes_and_caches() {
        let (scene, group, _a) = setup();
        let mut cache = BoolPathCache::new();
        let p = cache.get_or_compute(&scene, group);
        assert!(!p.subpaths.is_empty(), "union of overlapping squares");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_hit_when_subtree_unchanged() {
        let (scene, group, _a) = setup();
        let mut cache = BoolPathCache::new();
        let rev_before = scene.subtree_revision(group);
        let _ = cache.get_or_compute(&scene, group);
        let _ = cache.get_or_compute(&scene, group);
        // Second call should NOT have bumped anything.
        assert_eq!(scene.subtree_revision(group), rev_before);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_miss_when_descendant_geometry_changes() {
        let (mut scene, group, a) = setup();
        let mut cache = BoolPathCache::new();
        let _ = cache.get_or_compute(&scene, group);
        let rev_before = scene.subtree_revision(group);

        // Edit a descendant path → must bump group's subtree_rev.
        scene
            .set_path_data(a, square(0.0, 0.0, 20.0))
            .expect("set path");
        assert!(
            scene.subtree_revision(group) > rev_before,
            "descendant edit must cascade to ancestor subtree_rev"
        );

        // Next call must see a cache miss and recompute.
        let _ = cache.get_or_compute(&scene, group);
        // Still one entry, but with the new revision.
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn cache_miss_when_descendant_transform_changes() {
        let (mut scene, group, a) = setup();
        let mut cache = BoolPathCache::new();
        let _ = cache.get_or_compute(&scene, group);
        let rev_before = scene.subtree_revision(group);

        // Translate a descendant → compositing result changes → must invalidate.
        scene
            .set_transform(a, vector_geom::Affine::translate(100.0, 0.0))
            .expect("set transform");
        assert!(scene.subtree_revision(group) > rev_before);

        let _ = cache.get_or_compute(&scene, group);
        assert_eq!(cache.len(), 1);
    }
}
