//! Cross-scene merging.
//!
//! [`merge_as_group`] is the primitive used to insert one [`Scene`]'s
//! contents into another as a single child group, preserving the source's
//! gradient/pattern definitions and rewriting any inter-node `NodeId`
//! references (paint refs, pattern content) to point at the new copies.
//!
//! Used by the editor's raster-trace dialog (which builds a fresh
//! tracing-result scene each frame and merges it into the live document
//! as a preview group). Also a generally useful primitive for
//! paste-from-clipboard, "import as group", and similar flows that need
//! to graft a sub-document onto an existing tree without flattening or
//! breaking references.

use std::collections::HashMap;

use crate::node::{GroupKind, Node, NodeData, TextData};
use crate::paint::{Paint, PaintRef};
use crate::scene::{NodeId, Scene};
use crate::style::Style;

/// Merge `source` into `dest` as a single child group of `dest_parent`,
/// copying paint/pattern definitions into `dest.defs()` and rewriting
/// every inter-node `NodeId` reference to point at the new copies.
///
/// Returns the new group's `NodeId`, or `None` if `dest_parent` doesn't
/// exist in `dest`.
///
/// The merge proceeds in two passes:
/// 1. **Copy.** Every node in `source` (defs subtree first, then visible
///    content under `source.root()`) is cloned into `dest`. Each clone's
///    `children` vec is cleared and rebuilt by recursive insertion, so
///    every old↔new id pair lands in the returned map.
/// 2. **Remap.** With the id map complete, every newly-inserted node
///    that carries a `NodeId` reference (path styles, boolean-group
///    styles, text styles — via `PaintRef::Ref` — and `Pattern.content`)
///    is rewritten to use the new ids.
///
/// Two passes are required because pattern content groups live in defs
/// alongside the patterns that reference them: walking in any single
/// order would forward-reference an id that doesn't exist yet.
pub fn merge_as_group(
    dest: &mut Scene,
    dest_parent: NodeId,
    source: &Scene,
    label: impl Into<String>,
) -> Option<NodeId> {
    dest.get(dest_parent)?;

    let mut id_map: HashMap<NodeId, NodeId> = HashMap::new();

    // 1a. Copy source's defs subtree into dest's defs.
    let dest_defs = dest.defs();
    let source_defs = source.defs();
    let defs_children: Vec<NodeId> = source
        .get(source_defs)
        .map(|n| n.children.clone())
        .unwrap_or_default();
    for child_id in defs_children {
        copy_subtree(source, child_id, dest, dest_defs, &mut id_map);
    }

    // 1b. Create the new group and copy source's visible content into it.
    //     `source.root()`'s children include `source.defs()` itself (it's
    //     a child of root by construction); we skip that, since defs were
    //     handled above.
    let new_group = dest.insert(dest_parent, Node::group(label))?;
    let source_root = source.root();
    let root_children: Vec<NodeId> = source
        .get(source_root)
        .map(|n| n.children.clone())
        .unwrap_or_default();
    for child_id in root_children {
        if child_id == source_defs {
            continue;
        }
        copy_subtree(source, child_id, dest, new_group, &mut id_map);
    }

    // 2. Remap NodeId references inside the copies.
    remap_references(dest, &id_map);

    Some(new_group)
}

/// Recursively copy `src_id` and its descendants from `source` into
/// `dest`, parented under `dest_parent`. Each clone's `children` field
/// is cleared and the parent-child relationships are rebuilt by
/// recursive insertion, so the dest scene's revision counters are
/// updated correctly. Records every old→new mapping in `id_map`.
fn copy_subtree(
    source: &Scene,
    src_id: NodeId,
    dest: &mut Scene,
    dest_parent: NodeId,
    id_map: &mut HashMap<NodeId, NodeId>,
) {
    let Some(src_node) = source.get(src_id) else {
        return;
    };
    let mut copy = src_node.clone();
    copy.children.clear();
    let Some(new_id) = dest.insert(dest_parent, copy) else {
        return;
    };
    id_map.insert(src_id, new_id);
    let child_ids = src_node.children.clone();
    for child_id in child_ids {
        copy_subtree(source, child_id, dest, new_id, id_map);
    }
}

/// Snapshot of a per-node mutation needed to finish the merge — built
/// from an immutable borrow of `dest`, applied through `dest`'s typed
/// setters once the borrow has been dropped.
enum RemapAction {
    SetStyle(Style),
    SetGroupKind(GroupKind),
    SetText(TextData),
    SetPatternContent(NodeId),
    None,
}

/// Walk every newly-inserted node and rewrite any `NodeId` references
/// inside its data to use the post-merge ids. Mutations go through
/// `dest`'s typed setters / edit guard so revision counters bump
/// correctly — a path whose fill now points at a copied gradient must
/// have its tessellation cache invalidated.
fn remap_references(dest: &mut Scene, id_map: &HashMap<NodeId, NodeId>) {
    let ids: Vec<NodeId> = id_map.values().copied().collect();
    for new_id in ids {
        let action = match dest.get(new_id) {
            Some(node) => match &node.data {
                NodeData::Path { style, .. } => {
                    let mut s = style.clone();
                    if remap_style(&mut s, id_map) {
                        RemapAction::SetStyle(s)
                    } else {
                        RemapAction::None
                    }
                }
                NodeData::Group {
                    kind: GroupKind::Boolean { style, op },
                    ..
                } => {
                    let mut s = style.clone();
                    if remap_style(&mut s, id_map) {
                        RemapAction::SetGroupKind(GroupKind::Boolean { op: *op, style: s })
                    } else {
                        RemapAction::None
                    }
                }
                NodeData::Text(t) => {
                    let mut nt = t.clone();
                    if remap_style(&mut nt.style, id_map) {
                        RemapAction::SetText(nt)
                    } else {
                        RemapAction::None
                    }
                }
                NodeData::Paint(Paint::Pattern(pat)) => match id_map.get(&pat.content) {
                    Some(&new_content) if new_content != pat.content => {
                        RemapAction::SetPatternContent(new_content)
                    }
                    _ => RemapAction::None,
                },
                _ => RemapAction::None,
            },
            None => RemapAction::None,
        };
        match action {
            RemapAction::SetStyle(s) => {
                let _ = dest.set_style(new_id, s);
            }
            RemapAction::SetGroupKind(k) => {
                let _ = dest.set_group_kind(new_id, k);
            }
            RemapAction::SetText(t) => {
                let _ = dest.set_text_data(new_id, t);
            }
            RemapAction::SetPatternContent(nid) => {
                if let Some(mut guard) = dest.edit(new_id)
                    && let NodeData::Paint(Paint::Pattern(p)) = &mut guard.data
                {
                    p.content = nid;
                }
            }
            RemapAction::None => {}
        }
    }
}

/// Rewrite `style.fill.paint` and `style.stroke.paint` if either refers
/// to an id in `id_map`. Returns true iff any change was made.
fn remap_style(style: &mut Style, id_map: &HashMap<NodeId, NodeId>) -> bool {
    let mut changed = false;
    if let Some(fill) = &mut style.fill
        && remap_paint_ref(&mut fill.paint, id_map)
    {
        changed = true;
    }
    if let Some(stroke) = &mut style.stroke
        && remap_paint_ref(&mut stroke.paint, id_map)
    {
        changed = true;
    }
    changed
}

fn remap_paint_ref(p: &mut PaintRef, id_map: &HashMap<NodeId, NodeId>) -> bool {
    if let PaintRef::Ref(old) = p
        && let Some(&new_id) = id_map.get(old)
    {
        *p = PaintRef::Ref(new_id);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paint::{ColorStop, Gradient, GradientKind, InterpolationSpace, SpreadMethod};
    use crate::style::{Fill, FillRule};
    use locus_geom::{Affine, Color, Path, Point, Segment, SubPath, VertexMode};

    fn line_path() -> Path {
        let mut sp = SubPath::new(Point::new(0.0, 0.0));
        sp.push_segment(
            Segment::Line {
                to: Point::new(10.0, 10.0),
            },
            VertexMode::Corner,
        );
        let mut path = Path::new();
        path.subpaths.push(sp);
        path
    }

    fn linear_gradient() -> Gradient {
        Gradient {
            kind: GradientKind::Linear {
                start: Point::new(0.0, 0.0),
                end: Point::new(10.0, 0.0),
            },
            stops: vec![
                ColorStop {
                    offset: 0.0,
                    color: Color::BLACK,
                },
                ColorStop {
                    offset: 1.0,
                    color: Color::WHITE,
                },
            ],
            interpolation: InterpolationSpace::default(),
            transform: Affine::IDENTITY,
            spread: SpreadMethod::default(),
        }
    }

    #[test]
    fn merge_simple_paths_into_group() {
        // Source: a single solid-colour path.
        let mut source = Scene::new();
        let p = source
            .insert(source.root(), Node::path("p1", line_path()))
            .unwrap();
        assert!(source.get(p).is_some());

        // Dest: empty scene with one existing path.
        let mut dest = Scene::new();
        let dest_root = dest.root();
        let _existing = dest
            .insert(dest_root, Node::path("existing", line_path()))
            .unwrap();
        let dest_root_children_before = dest.get(dest_root).unwrap().children.len();

        let new_group = merge_as_group(&mut dest, dest_root, &source, "Traced").unwrap();

        // Dest root gained exactly one child (the new group).
        assert_eq!(
            dest.get(dest_root).unwrap().children.len(),
            dest_root_children_before + 1
        );
        // The new group has the merged path as its child.
        assert_eq!(dest.get(new_group).unwrap().children.len(), 1);
        let merged_path_id = dest.get(new_group).unwrap().children[0];
        let merged = dest.get(merged_path_id).unwrap();
        assert_eq!(merged.label, "p1");
        assert!(matches!(merged.data, NodeData::Path { .. }));
    }

    #[test]
    fn merge_remaps_paint_refs_to_copied_gradients() {
        // Source scene: a gradient in defs, a path that references it.
        let mut source = Scene::new();
        let source_defs = source.defs();
        let source_root = source.root();
        let grad_id = source
            .insert(
                source_defs,
                Node {
                    label: "g".into(),
                    transform: Affine::IDENTITY,
                    data: NodeData::Paint(Paint::Gradient(linear_gradient())),
                    children: Vec::new(),
                    visible: true,
                    locked: false,
                },
            )
            .unwrap();
        let path_id = source
            .insert(source_root, {
                let mut n = Node::path("p", line_path());
                if let NodeData::Path { style, .. } = &mut n.data {
                    *style = Style {
                        fill: Some(Fill {
                            paint: PaintRef::Ref(grad_id),
                            rule: FillRule::NonZero,
                            opacity: 1.0,
                        }),
                        stroke: None,
                    };
                }
                n
            })
            .unwrap();
        assert!(source.get(path_id).is_some());

        // Merge into a fresh dest scene.
        let mut dest = Scene::new();
        let dest_root = dest.root();
        let dest_defs = dest.defs();
        let new_group = merge_as_group(&mut dest, dest_root, &source, "Imported").unwrap();

        // The path's fill should reference a gradient under dest.defs(),
        // not the original source NodeId (which doesn't exist in dest).
        let merged_path_id = dest.get(new_group).unwrap().children[0];
        let merged_path = dest.get(merged_path_id).unwrap();
        let NodeData::Path { style, .. } = &merged_path.data else {
            panic!("merged node should be a Path");
        };
        let fill_paint = &style.fill.as_ref().unwrap().paint;
        let PaintRef::Ref(new_grad_id) = fill_paint else {
            panic!("merged path's fill should still be a Ref, got {fill_paint:?}");
        };
        // The new id must *not* equal the source id (different scene
        // means a different SlotMap allocator, but we can't rely on that
        // alone — generationally, source's id might collide). What we
        // *can* assert is that the new id resolves to a gradient node
        // under dest.defs(), and that the original source id either
        // doesn't exist in dest or doesn't point at a gradient.
        let new_grad_node = dest
            .get(*new_grad_id)
            .expect("new gradient id must resolve in dest");
        assert!(matches!(
            new_grad_node.data,
            NodeData::Paint(Paint::Gradient(_))
        ));
        assert_eq!(dest.parent(*new_grad_id), Some(dest_defs));
    }

    #[test]
    fn merge_returns_none_for_unknown_parent() {
        // SlotMap reuses index slots across new scenes, so a NodeId from
        // one scene can collide with a valid id in another. To get a
        // genuinely invalid id, allocate then remove a node — the
        // generational key bumps, so the stale id no longer resolves.
        let source = Scene::new();
        let mut dest = Scene::new();
        let dest_root = dest.root();
        let stale = dest.insert(dest_root, Node::group("tmp")).unwrap();
        dest.remove(stale);

        let result = merge_as_group(&mut dest, stale, &source, "x");
        assert!(
            result.is_none(),
            "merging under stale (removed) parent must fail"
        );
    }

    #[test]
    fn merge_preserves_nested_groups() {
        // Source: root -> outer group -> path
        let mut source = Scene::new();
        let source_root = source.root();
        let outer = source.insert(source_root, Node::group("outer")).unwrap();
        let _inner = source
            .insert(outer, Node::path("inner", line_path()))
            .unwrap();

        let mut dest = Scene::new();
        let dest_root = dest.root();
        let new_group = merge_as_group(&mut dest, dest_root, &source, "Traced").unwrap();

        // new_group has one child (the copy of `outer`), which has one child (the path).
        let outer_copy_id = dest.get(new_group).unwrap().children[0];
        let outer_copy = dest.get(outer_copy_id).unwrap();
        assert_eq!(outer_copy.label, "outer");
        assert_eq!(outer_copy.children.len(), 1);
        let path_copy = dest.get(outer_copy.children[0]).unwrap();
        assert_eq!(path_copy.label, "inner");
        assert!(matches!(path_copy.data, NodeData::Path { .. }));
    }
}
