// Text shaping and glyph outline extraction.
// Wraps rustybuzz + ttf-parser into our own API that returns
// vector-geom Path segments for a (string, font, size) tuple.

// TODO: implement text shaping pipeline
//   1. Load font with ttf-parser
//   2. Shape text with rustybuzz (handles kerning, ligatures, complex scripts)
//   3. Extract glyph outlines as our Segment types
//   4. Position glyphs along baseline
