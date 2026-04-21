//! Per-path tessellation cache.
//!
//! Path and text nodes tessellate to the same mesh as long as their
//! local-space geometry doesn't change. That's exactly what the scene's
//! `geometry_rev` tracks: it bumps on path-data, style, or text-data
//! edits, but *not* on transform edits. Caching keyed on `geometry_rev`
//! therefore lets transform dragging reuse tessellation output verbatim.
//!
//! Boolean groups are intentionally *not* cached here — their own
//! `geometry_rev`/`subtree_rev` combo over-invalidates on group-own
//! transform edits, and tessellation of an already-cached computed path
//! is cheap compared to the `i_overlay` boolean that Phase B eliminated.
//!
//! Cached meshes store vertices in the node's local coordinate space.
//! The renderer rewrites each vertex's `path_id` on upload so the
//! vertex shader can look up the correct world transform from the
//! `transforms` storage buffer.
//!
//! Backed by a `SecondaryMap`, so cache entries are dropped automatically
//! when their node is removed from the scene.

use slotmap::SecondaryMap;
use vector_scene::NodeId;
use vector_tess::{TessellatedMesh, Vertex};

pub(crate) struct CachedMesh {
    pub geometry_rev: u64,
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

/// Cache of tessellated meshes, keyed on `NodeId` and invalidated when
/// the node's `Scene::geometry_revision` advances.
#[derive(Default)]
pub(crate) struct TessCache {
    entries: SecondaryMap<NodeId, CachedMesh>,
    hits: u32,
    misses: u32,
}

/// Statistics from the most recent `prepare()` call.
#[derive(Copy, Clone, Debug, Default)]
pub struct TessCacheStats {
    pub hits: u32,
    pub misses: u32,
}

impl TessCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset per-frame counters. Called at the start of a `prepare()` run.
    pub fn reset_stats(&mut self) {
        self.hits = 0;
        self.misses = 0;
    }

    pub fn stats(&self) -> TessCacheStats {
        TessCacheStats {
            hits: self.hits,
            misses: self.misses,
        }
    }

    /// Fetch or insert the cached mesh for `id`.
    ///
    /// If the cached entry's `geometry_rev` matches `current_rev`, returns
    /// the cached entry (hit). Otherwise runs `tessellate` to produce a
    /// fresh mesh, stores it, and returns that (miss).
    pub fn get_or_insert_with<F>(
        &mut self,
        id: NodeId,
        current_rev: u64,
        tessellate: F,
    ) -> &CachedMesh
    where
        F: FnOnce() -> TessellatedMesh,
    {
        let stale = self
            .entries
            .get(id)
            .is_none_or(|c| c.geometry_rev != current_rev);
        if stale {
            self.misses += 1;
            let mesh = tessellate();
            self.entries.insert(
                id,
                CachedMesh {
                    geometry_rev: current_rev,
                    vertices: mesh.vertices,
                    indices: mesh.indices,
                },
            );
        } else {
            self.hits += 1;
        }
        &self.entries[id]
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_tess::TessellatedMesh;

    fn fake_mesh(n_verts: usize) -> TessellatedMesh {
        TessellatedMesh {
            vertices: (0..n_verts)
                .map(|i| Vertex::solid([i as f32, 0.0], [0.0, 0.0, 0.0, 1.0]))
                .collect(),
            indices: (0..n_verts as u32).collect(),
        }
    }

    #[test]
    fn first_call_misses_and_caches() {
        // NodeIds come from a real Scene; use a scene so the test is honest.
        use vector_geom::{Path, Point, Segment, SubPath, VertexMode};
        use vector_scene::{Node, Scene};

        let mut path = Path::new();
        path.subpaths.push(SubPath {
            start: Point::new(0.0, 0.0),
            segments: vec![
                Segment::Line {
                    to: Point::new(1.0, 0.0),
                },
                Segment::Line {
                    to: Point::new(1.0, 1.0),
                },
            ],
            closed: true,
            vertex_modes: vec![VertexMode::Corner; 3],
        });
        let mut scene = Scene::new();
        let id = scene.insert(scene.root(), Node::path("p", path)).unwrap();

        let mut cache = TessCache::new();
        cache.reset_stats();
        let _ = cache.get_or_insert_with(id, scene.geometry_revision(id), || fake_mesh(3));
        assert_eq!(cache.stats().hits, 0);
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.len(), 1);

        cache.reset_stats();
        let _ =
            cache.get_or_insert_with(id, scene.geometry_revision(id), || panic!("must be a hit"));
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 0);
    }

    #[test]
    fn geometry_rev_change_invalidates() {
        use vector_geom::{Path, Point, Segment, SubPath, VertexMode};
        use vector_scene::{Node, Scene};

        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", Path::new()))
            .unwrap();

        let mut cache = TessCache::new();
        cache.reset_stats();
        let _ = cache.get_or_insert_with(id, scene.geometry_revision(id), || fake_mesh(1));

        // Edit path → geometry_rev bumps.
        let mut new_path = Path::new();
        new_path.subpaths.push(SubPath {
            start: Point::new(0.0, 0.0),
            segments: vec![Segment::Line {
                to: Point::new(2.0, 0.0),
            }],
            closed: false,
            vertex_modes: vec![VertexMode::Corner; 2],
        });
        scene.set_path_data(id, new_path).unwrap();

        cache.reset_stats();
        let _ = cache.get_or_insert_with(id, scene.geometry_revision(id), || fake_mesh(2));
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn transform_edit_does_not_invalidate() {
        use vector_geom::{Affine, Path};
        use vector_scene::{Node, Scene};

        let mut scene = Scene::new();
        let id = scene
            .insert(scene.root(), Node::path("p", Path::new()))
            .unwrap();

        let mut cache = TessCache::new();
        cache.reset_stats();
        let _ = cache.get_or_insert_with(id, scene.geometry_revision(id), || fake_mesh(1));

        // Transform edit: subtree_rev bumps but geometry_rev does NOT.
        scene
            .set_transform(id, Affine::translate(100.0, 0.0))
            .unwrap();

        cache.reset_stats();
        let _ = cache.get_or_insert_with(id, scene.geometry_revision(id), || {
            panic!("transform-only edit must not invalidate tess cache")
        });
        assert_eq!(cache.stats().hits, 1);
        assert_eq!(cache.stats().misses, 0);
    }

    #[test]
    fn n_paths_one_transform_edit_yields_n_minus_1_hits() {
        use vector_geom::{Affine, Path};
        use vector_scene::{Node, Scene};

        let mut scene = Scene::new();
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = scene
                .insert(scene.root(), Node::path(format!("p{i}"), Path::new()))
                .unwrap();
            ids.push(id);
        }

        // Prime the cache: all 5 are misses.
        let mut cache = TessCache::new();
        cache.reset_stats();
        for &id in &ids {
            let _ = cache.get_or_insert_with(id, scene.geometry_revision(id), || fake_mesh(1));
        }
        assert_eq!(cache.stats().misses, 5);
        assert_eq!(cache.stats().hits, 0);

        // Transform-edit exactly one of them.
        scene
            .set_transform(ids[2], Affine::translate(50.0, 0.0))
            .unwrap();

        // Second frame: every cache lookup should be a hit.
        cache.reset_stats();
        for &id in &ids {
            let _ = cache.get_or_insert_with(id, scene.geometry_revision(id), || {
                panic!("transform-only edit must not invalidate")
            });
        }
        assert_eq!(cache.stats().hits, 5);
        assert_eq!(cache.stats().misses, 0);
    }
}
