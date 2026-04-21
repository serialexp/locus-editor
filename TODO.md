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
- [x] SVG import smooth junction detection — post-pass on imported subpaths detects collinear ctrl2/ctrl1 pairs at junctions, classifies as Smooth (collinear, different lengths) or Symmetric (collinear + equidistant), handles closed-path wrap-around
- [x] Vertex constraint enforcement on closed-path wrap-around during drag — enforce_vertex_constraint now wraps segment indices on closed paths (first↔last segment pair)

- [x] Properties panel editable fields — DragValue widgets wired to scene: Object mode X/Y (translate), W/H (scale), Node mode vertex X/Y (local-space delta via inverse world transform), all with undo support
- [x] Object transforms from properties panel — dragging X/Y moves objects, dragging W/H scales (single selection), undo via SetTransform commands

- [x] Text tool — click to create/edit text, typing with cursor, backspace/delete, arrow keys, Home/End, Enter/Escape to commit, undo/redo support, caret rendering, I-beam cursor icon

- [x] Structure panel — context menu (right-click): New Group, Group Selection, Ungroup, Delete; "Add Group" and "Group Selection" buttons in header; drag-and-drop reparent through undo system
- [x] Node renaming — editable Name field in Properties panel for any selected node
- [x] Rotation — interactive rotation via corner zone outside bbox, properties panel rotation field (degrees), `Affine::rotate_around` for rotation about center
- [x] Scale handles — 8-handle bbox pattern (4 corners + 4 edge midpoints), absolute scale from original transforms, resize cursors per handle direction

## Pending
- [ ] Flatten/bake transforms — manual action to push non-translate transform into vertices (paths) or font size (text), leaving transform as translate-only
- [ ] Text tool — font selection UI in properties panel (ComboBox with system fonts)
- [ ] Text tool — text selection (Shift+arrow, click-drag)
- [ ] Text tool — multi-line text support
- [x] Multi-select in structure panel — Shift/Ctrl+click for additive selection (toggle behavior matching canvas)
- [x] Vertex mode toggle buttons in properties panel — Corner/Smooth/Symmetric selectable_labels replacing text display
- [x] Quad→Cubic upgrade in ensure_cubic_handles — degree elevation when switching vertex mode from Corner
- [x] Canvas context menus — right-click on vertex (mode toggle + delete) or segment (Line/Quad/Cubic conversion + insert vertex)
- [x] Segment type conversion — convert_segment_to_line/quad/cubic with undo support
- [ ] Snap to points/vertices — snap to existing geometry, not just grid

### Raster tracing (`vector-trace`)
- [ ] Preset/parameter tuning UI — egui dialog exposing TracePreset (Bw/Poster/Photo) and the key vtracer knobs (filter_speckle, color_precision, corner_threshold, splice_threshold, layer_difference, path simplify mode). Currently hard-coded to Poster default.
- [ ] Async tracing — run `vector_trace::trace_image_bytes` off the UI thread (std::thread + channel, or a simple worker). Large images block the UI for seconds.
- [ ] Live preview while tuning — reduced-resolution preview that re-traces on parameter change (depends on async tracing + parameter UI).
- [ ] Centerline tracing — v2 feature for line art / technical drawings where strokes should become single stroked paths instead of filled outlines. No good Rust crate exists; custom skeletonization (medial axis) required.
