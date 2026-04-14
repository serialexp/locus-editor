# TODO

- [x] Wire undo/redo to UI — Ctrl+Z/Ctrl+Shift+Z/Ctrl+Y, Edit menu, all tools wired
- [ ] Arc tessellation — `Segment::Arc` falls back to a straight line in the tessellator; need `lyon_geom::SvgArc` conversion
- [ ] World transforms in renderer — computed during scene walk but not applied to vertices
- [ ] Text rendering — vector-text is a stub (rustybuzz + ttf-parser deps wired, no implementation)
- [ ] Gradient/pattern rendering — `PaintRef::Ref` falls back to black
- [ ] Stroke dash patterns — not rendered yet
