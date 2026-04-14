# Current State

## What's working

- Full editor with wgpu rendering, egui UI panels, pan/zoom camera
- **Select tool**: click to select nodes/vertices, drag to move, rubber-band selection, handle manipulation (cubic/quad control points)
- **Pen tool**: Illustrator-style click (corner) / click-drag (smooth curve), close path, right-click undo, Enter/Escape finish
- **Shape tool**: rectangle/ellipse drawing
- **SVG import** via usvg (solid colors, paths, groups, transforms)
- **SVG export**: all 4 segment types (L/Q/C/A), fill/stroke, transforms, viewBox
- **File dialogs**: Open/Save SVG via rfd
- **Delete**: vertex deletion (backspace/delete with vertices selected) and object deletion
- **Double-click** to insert points on path edges
- **Arc geometry**: proper SVG spec F.6 endpoint-to-center conversion, eval, closest point, split
- **Undo/redo**: Ctrl+Z / Ctrl+Shift+Z / Ctrl+Y, Edit menu buttons
  - All operations undoable: pen path creation, shape creation, object deletion, vertex deletion, vertex dragging, insert point on edge, style changes
  - Command system: Insert, InsertSubtree, Delete, SetPathData, SetStyle, SetTransform, Batch
  - Subtree snapshot/restore for faithful undo of hierarchical deletions
  - History cleared on file open / drag-and-drop load
  - Undo/redo disabled during active pen/shape drawing
- **CI**: GitHub Actions (format, clippy -D warnings, build, test) + cross-platform build matrix
- **Release**: GitHub Actions on v* tags, builds for Linux/macOS/Windows
- **Install script**: platform detection, binary install

## Known TODOs

1. **Arc tessellation** — `Segment::Arc` falls back to a straight line in the tessellator; need lyon_geom::SvgArc conversion
2. **World transforms in renderer** — computed but not applied to vertices
3. **Text rendering** — vector-text is a stub (rustybuzz + ttf-parser deps wired but no implementation)
4. **Gradient/pattern rendering** — PaintRef::Ref falls back to black
5. **Stroke rendering fidelity** — stroke tessellation works but no dash patterns in renderer yet
