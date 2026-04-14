# TODO

- [x] Wire undo/redo to UI — Ctrl+Z/Ctrl+Shift+Z/Ctrl+Y, Edit menu, all tools wired
- [x] Arc tessellation — already implemented via `lyon_geom::SvgArc::for_each_cubic_bezier`
- [x] World transforms in renderer — applied to tessellated vertices, handles, hit testing, bounds, and vertex dragging
- [x] Text rendering — shaping pipeline (rustybuzz + ttf-parser), bundled Liberation Sans font, SVG import/export, renderer integration
- [x] System font matching — fontdb-backed FontDb with CSS-like queries, system font loading, cached parsed fonts, fallback to bundled Liberation Sans
- [x] Text bounding boxes for selection/hit-testing — shared global_font(), content_bounds and hit-testing handle Text nodes
- [x] Gradient rendering — fragment shader evaluation, SVG import/export, linear + radial + spread methods
- [ ] Pattern rendering — patterns not yet supported
- [x] Stroke dash patterns — dash expansion before tessellation, cap/join/miter passed through
