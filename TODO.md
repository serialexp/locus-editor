# TODO

- [x] Wire undo/redo to UI — Ctrl+Z/Ctrl+Shift+Z/Ctrl+Y, Edit menu, all tools wired
- [x] Arc tessellation — already implemented via `lyon_geom::SvgArc::for_each_cubic_bezier`
- [x] World transforms in renderer — applied to tessellated vertices, handles, hit testing, bounds, and vertex dragging
- [ ] Text rendering — vector-text is a stub (rustybuzz + ttf-parser deps wired, no implementation)
- [ ] Gradient/pattern rendering — `PaintRef::Ref` falls back to black
- [ ] Stroke dash patterns — not rendered yet
