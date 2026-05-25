# Locus

A GPU-accelerated SVG/vector graphics editor built from first principles in Rust. Intended as a modern, fast alternative to Inkscape.

## Features

- **GPU rendering** via wgpu — custom shader pipeline, no framework crates
- **Full SVG segment types** — lines, quadratic/cubic beziers, and arcs preserved (no normalization to cubics), for round-trip fidelity
- **Gradient rendering** — linear and radial gradients with spread methods (pad, reflect, repeat), evaluated per-pixel in the fragment shader
- **Pattern rendering** — SVG `<pattern>` support via render-to-texture with nested pattern resolution and cycle detection
- **Stroke dash patterns** — dash expansion before tessellation with cap/join/miter support
- **Text rendering** — font shaping via rustybuzz + ttf-parser, system font matching with bundled Liberation Sans fallback
- **Interactive editing** — select tool with node/object modes, pen tool for path creation, vertex mode cycling (corner/smooth/symmetric)
- **Undo/redo** — full command history for all editing operations
- **SVG import/export** — round-trip capable, preserving gradients, patterns, text, transforms, and group hierarchy
- **Grid snapping** — configurable grid with visual overlay
- **egui UI** — properties panel, structure panel with visibility toggles, menus

## Install

### From release (Linux/macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/serialexp/locus-editor/main/install.sh | bash
```

### From source

Requires [Rust](https://rustup.rs/) 1.92+ and system dependencies for windowing:

```sh
# Ubuntu/Debian
sudo apt-get install -y libxkbcommon-dev libx11-dev libxcb1-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev \
  libwayland-dev libgtk-3-dev

# Build and run
cargo run -p locus-editor --release
```

## Usage

```sh
# Launch the editor
locus-editor

# Open an SVG file
locus-editor path/to/file.svg
```

## Development

```sh
just run            # debug build + run
just run-release    # release build + run
just test           # run all tests
just ci             # full CI check (fmt + clippy + test)
```

## Architecture

```
bin/locus-editor/     Main GUI binary (winit + wgpu + egui)
crates/
  locus-geom/         Point, Vec2, Affine, Bounds, Segment, Path, Color
  locus-scene/        Node tree (SlotMap-based), Style, Paint, PaintRef
  locus-svg/          SVG import (usvg) / export
  locus-text/         Text shaping (rustybuzz + ttf-parser)
  locus-tess/         Path tessellation via lyon (fill + stroke)
  locus-render/       wgpu pipeline, shaders, scene rendering
  locus-ops/          Undo/redo command history
  locus-tools/        Editing tools (select, pen)
```

## License

MIT
