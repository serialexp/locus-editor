# TODO

## Completed

- [x] Wire undo/redo to UI — Ctrl+Z/Ctrl+Shift+Z/Ctrl+Y, Edit menu, all tools wired
- [x] Arc tessellation — already implemented via `lyon_geom::SvgArc::for_each_cubic_bezier`
- [x] World transforms in renderer — applied to tessellated vertices, handles, hit testing, bounds, and vertex dragging
- [x] Text rendering — shaping pipeline (rustybuzz + ttf-parser), bundled Liberation Sans font, SVG import/export, renderer integration
- [x] System font matching — fontdb-backed FontDb with CSS-like queries, system font loading, cached parsed fonts, fallback to bundled Liberation Sans
- [x] Text bounding boxes for selection/hit-testing — shared global_font(), content_bounds and hit-testing handle Text nodes
- [x] Gradient rendering — fragment shader evaluation, SVG import/export, linear + radial + spread methods
- [x] Stroke dash patterns — dash expansion before tessellation, cap/join/miter passed through
- [x] VertexMode (Corner/Smooth/Symmetric) — explicit per-anchor mode, double-click to cycle, constraint enforcement on handle drag
- [x] Handle lines — solid for symmetric, dashed for asymmetric, between control points and anchors
- [x] Origin crosshair — rendered at (0,0) in reddish-orange
- [x] Properties panel redesign — CollapsingHeaders, Grid layout, DragValue widgets for transform/bounds
- [x] Structure panel click-to-select — clicking a node in the tree selects it on canvas
- [x] Structure panel visibility toggle — eye icon button to hide/show nodes
- [x] Ghost vertex preview — semi-transparent handle shown on edge hover where double-click would insert
- [x] Grid snapping — configurable grid size, View menu toggle, visual grid synced to snap spacing
- [x] SVG import group flattening — trivial wrapper groups (identity transform, no clip/mask) are skipped
- [x] Fix pointer state desync — is_left_down/is_panning always cleared on button release regardless of pointer location
- [x] Fix egui text selection — disabled selectable_labels, use is_pointer_over_egui() for canvas hover suppression
- [x] Cubic handle creation on mode switch — Lines upgraded to Cubics with handles at 1/3 segment length, closed-path wrap-around
- [x] Pattern rendering — Pattern paint type in scene graph, render-to-texture pipeline (per-tile rasterization into texture array, fragment shader sampling), SVG import/export with deduplication, nested pattern support with topological ordering and cycle detection (per SVG spec)

## Pending
- [ ] Properties panel editable fields — DragValue widgets for X/Y/W/H currently read-only; need to wire mutations to scene + undo
- [ ] SVG import smooth junction detection — detect collinear ctrl2/ctrl1 pairs at junctions and set VertexMode::Smooth
- [ ] Vertex constraint enforcement on closed-path wrap-around during drag — enforce_vertex_constraint doesn't handle the last↔first segment pair
- [ ] Text tool — text editing, cursor, font selection UI
- [ ] Object transforms from properties panel — dragging X/Y/W/H should move/resize objects
- [ ] Multi-select in structure panel — Shift/Ctrl+click for additive selection
- [ ] Snap to points/vertices — snap to existing geometry, not just grid
