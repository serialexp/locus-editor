# Current Task: Initial Project Scaffold

## Status: Complete — scaffold compiles and builds

## What was done

Set up the full workspace with 8 library crates + 1 binary:

```
crates/
  vector-geom/    ✅ Point, Vec2, Affine, Bounds, Segment, Path, SubPath, Color (linear RGBA)
  vector-scene/   ✅ Node, NodeData, Scene (SlotMap-based), Style, Paint, PaintRef, stable IDs
  vector-svg/     ✅ SVG import via usvg (solid colors, paths, groups), export stub
  vector-text/    ⬜ Stub only (rustybuzz + ttf-parser deps wired)
  vector-tess/    ✅ Path tessellation via lyon (fill + stroke), arc fallback is line-to (TODO)
  vector-render/  ✅ wgpu pipeline, shader, ortho projection, scene walk + tessellate + draw
  vector-ops/     ✅ Command enum (Insert/Delete/Batch) + undo/redo History
  vector-tools/   ⬜ Stub only

bin/
  vector-editor/  ✅ winit + wgpu + egui app loop, menu bar, tool/layer/property panels,
                     demo triangle, drag-and-drop SVG loading
```

## What works

- `cargo build` succeeds with only 1 dead_code warning (history field not yet wired)
- Running the binary opens a window with a steel-blue triangle rendered via wgpu
- Dropping an SVG file onto the window imports and displays it (solid-color paths only)
- egui panels render (menu bar, tool sidebar, properties, layers)

## Known TODOs (next steps)

1. **Arc tessellation** — `Segment::Arc` currently falls back to a straight line. Need to use `lyon_geom::SvgArc` for proper arc-to-cubic conversion in the tessellator.
2. **World transforms** — scene walk computes world transforms but the renderer doesn't apply them to vertices yet. Need to either bake into vertices or use per-draw push constants.
3. **Text** — vector-text is a stub. Need to implement: font loading, text shaping via rustybuzz, glyph outline extraction.
4. **Tools** — vector-tools is a stub. Start with Select tool (click to select, drag to move).
5. **Gradient/pattern rendering** — PaintRef::Ref falls back to black. Need gradient uniforms/textures.
6. **SVG export** — export_svg is a stub.
7. **Pan/zoom** — no camera controls yet.
