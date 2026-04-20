//! Text shaping and glyph outline extraction.
//!
//! Wraps `rustybuzz` (text shaping / kerning / ligatures), `ttf-parser`
//! (font parsing / glyph outline extraction) and `fontdb` (system font
//! matching) into an API that turns `(text, font_family, font_size)` into
//! a `vector_geom::Path` ready for tessellation.
//!
//! ## Font resolution
//!
//! [`FontDb`] loads system fonts and the bundled fallback (Liberation Sans).
//! A CSS-like query by family name resolves to a concrete font. Parsed fonts
//! are cached for the process lifetime (data is leaked to `'static` — fonts
//! are a finite, long-lived resource in an editor).
//!
//! The global instance ([`global_font_db`]) is initialized lazily on first
//! access.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use vector_geom::{Bounds, Path, Point, Segment, SubPath, VertexMode};

/// The embedded fallback font (Liberation Sans Regular, SIL OFL license).
/// Public so other crates (e.g. vector-svg) can load it into their own font databases.
pub static DEFAULT_FONT_DATA: &[u8] = include_bytes!("../fonts/LiberationSans-Regular.ttf");

// ── FontDb ──────────────────────────────────────────────────────────

/// A font database that resolves family names to parsed `Font` instances.
///
/// Wraps `fontdb::Database` for system font discovery and caching. Parsed
/// fonts are cached by `fontdb::ID` and kept alive for the process lifetime.
pub struct FontDb {
    db: fontdb::Database,
    /// Cached parsed fonts, keyed by fontdb face ID.
    /// The `Font<'static>` borrows from leaked `Box<[u8]>` data.
    cache: HashMap<fontdb::ID, Font<'static>>,
    /// The fallback font, always available.
    fallback: Font<'static>,
}

impl FontDb {
    /// Create a new font database, loading system fonts and the bundled fallback.
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();

        // Register the bundled fallback font so it participates in queries.
        db.load_font_data(DEFAULT_FONT_DATA.to_vec());

        let fallback =
            Font::from_bytes(DEFAULT_FONT_DATA).expect("embedded fallback font should be valid");

        log::info!(
            "FontDb initialized: {} font faces loaded",
            db.faces().count()
        );

        FontDb {
            db,
            cache: HashMap::new(),
            fallback,
        }
    }

    /// Resolve a font family name to a `Font`, falling back to the bundled
    /// default if the family is not found. The result is cached.
    pub fn resolve(&mut self, family: &str) -> &Font<'static> {
        // Map generic CSS family names.
        let fontdb_family = match family {
            "serif" => fontdb::Family::Serif,
            "sans-serif" => fontdb::Family::SansSerif,
            "monospace" => fontdb::Family::Monospace,
            "cursive" => fontdb::Family::Cursive,
            "fantasy" => fontdb::Family::Fantasy,
            _ => fontdb::Family::Name(family),
        };

        let query = fontdb::Query {
            families: &[fontdb_family, fontdb::Family::SansSerif],
            weight: fontdb::Weight(400),
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };

        if let Some(id) = self.db.query(&query) {
            if self.cache.contains_key(&id) {
                return &self.cache[&id];
            }

            // Extract font data from fontdb. We copy it out and leak it to
            // get a `'static` lifetime — fonts are loaded once and kept for
            // the duration of the editor process.
            let font = self.db.with_face_data(id, |data, face_index| {
                Font::from_bytes_with_index(Box::leak(data.to_vec().into_boxed_slice()), face_index)
            });

            if let Some(Some(font)) = font {
                self.cache.insert(id, font);
                return &self.cache[&id];
            }

            log::warn!(
                "Failed to parse font for family '{}', using fallback",
                family
            );
        } else {
            log::debug!("Font family '{}' not found, using fallback", family);
        }

        &self.fallback
    }

    /// Shape text with the font matching the given family name.
    pub fn shape_text(&mut self, text: &str, font_family: &str, font_size: f64) -> ShapedText {
        let font = self.resolve(font_family);
        font.shape_text(text, font_size)
    }

    /// Compute cursor positions with the font matching the given family name.
    pub fn cursor_positions(
        &mut self,
        text: &str,
        font_family: &str,
        font_size: f64,
    ) -> CursorPositions {
        let font = self.resolve(font_family);
        font.cursor_positions(text, font_size)
    }

    /// Return a sorted, deduplicated list of available font family names.
    pub fn family_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .db
            .faces()
            .flat_map(|face| face.families.iter().map(|(name, _)| name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Compute text bounds with the font matching the given family name.
    pub fn text_bounds(&mut self, content: &str, font_family: &str, font_size: f64) -> Bounds {
        if content.is_empty() {
            return Bounds::EMPTY;
        }
        let font = self.resolve(font_family);
        font.shape_text(content, font_size).bounds()
    }
}

impl Default for FontDb {
    fn default() -> Self {
        Self::new()
    }
}

// ── Global instance ─────────────────────────────────────────────────

/// Lazily-initialized global font database, shared across all callers.
/// Protected by a Mutex since `FontDb::resolve` takes `&mut self` (cache
/// mutation). Contention is negligible — font resolution is infrequent
/// relative to rendering.
static GLOBAL_FONT_DB: LazyLock<Mutex<FontDb>> = LazyLock::new(|| Mutex::new(FontDb::new()));

/// Get a lock on the global font database.
///
/// Use this for font resolution across all crates. The database is
/// initialized lazily on first access (loads system fonts).
pub fn global_font_db() -> std::sync::MutexGuard<'static, FontDb> {
    GLOBAL_FONT_DB.lock().expect("font db mutex poisoned")
}

/// Convenience: shape text using the global font database.
pub fn shape_text(text: &str, font_family: &str, font_size: f64) -> ShapedText {
    global_font_db().shape_text(text, font_family, font_size)
}

/// Convenience: compute text bounds using the global font database.
pub fn text_bounds(content: &str, font_family: &str, font_size: f64) -> Bounds {
    global_font_db().text_bounds(content, font_family, font_size)
}

/// Convenience: compute cursor positions using the global font database.
pub fn cursor_positions(text: &str, font_family: &str, font_size: f64) -> CursorPositions {
    global_font_db().cursor_positions(text, font_family, font_size)
}

// ── Font ────────────────────────────────────────────────────────────

/// A font loaded and ready for shaping + glyph extraction.
pub struct Font<'a> {
    ttf_face: ttf_parser::Face<'a>,
    buzz_face: rustybuzz::Face<'a>,
    units_per_em: f64,
}

impl<'a> Font<'a> {
    /// Parse a font from raw TrueType/OpenType data (face index 0).
    pub fn from_bytes(data: &'a [u8]) -> Option<Self> {
        Self::from_bytes_with_index(data, 0)
    }

    /// Parse a font from raw data with a specific face index (for .ttc collections).
    pub fn from_bytes_with_index(data: &'a [u8], face_index: u32) -> Option<Self> {
        let ttf_face = ttf_parser::Face::parse(data, face_index).ok()?;
        let buzz_face = rustybuzz::Face::from_slice(data, face_index)?;
        let units_per_em = ttf_face.units_per_em() as f64;
        Some(Font {
            ttf_face,
            buzz_face,
            units_per_em,
        })
    }

    /// Load the bundled default font (Liberation Sans Regular).
    pub fn default_font() -> Self {
        Self::from_bytes(DEFAULT_FONT_DATA).expect("embedded font should be valid")
    }

    /// Compute cursor X positions for each character boundary.
    ///
    /// Returns one X value per cursor position: before the first character,
    /// between each pair of characters, and after the last character.
    pub fn cursor_positions(&self, text: &str, font_size: f64) -> CursorPositions {
        if text.is_empty() {
            return CursorPositions {
                x_positions: vec![0.0],
            };
        }

        let scale = font_size / self.units_per_em;
        let char_count = text.chars().count();

        // Shape to get glyph advances.
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        let output = rustybuzz::shape(&self.buzz_face, &[], buffer);

        let infos = output.glyph_infos();
        let positions = output.glyph_positions();

        // Build a map from character index → cumulative advance after that char.
        // Glyphs reference their source character via the `cluster` field.
        let mut char_advances = vec![0.0_f64; char_count];

        for (info, pos) in infos.iter().zip(positions.iter()) {
            let cluster = info.cluster as usize;
            // Map byte offset (cluster) to char index.
            let char_idx = text[..cluster.min(text.len())].chars().count();
            let advance = pos.x_advance as f64 * scale;
            if char_idx < char_count {
                char_advances[char_idx] = advance;
            }
        }

        // Build cumulative positions.
        let mut x_positions = Vec::with_capacity(char_count + 1);
        x_positions.push(0.0);
        let mut x = 0.0;
        for adv in &char_advances {
            x += adv;
            x_positions.push(x);
        }

        CursorPositions { x_positions }
    }

    /// Shape a string of text and return glyph outlines as a single `Path`.
    ///
    /// The resulting path is positioned with the baseline at y=0, text
    /// flowing left-to-right from x=0. Scale is in the coordinate system
    /// where 1 unit = 1 SVG user unit (i.e. `font_size` determines height).
    pub fn shape_text(&self, text: &str, font_size: f64) -> ShapedText {
        let scale = font_size / self.units_per_em;
        let ascent = self.ttf_face.ascender() as f64 * scale;
        let descent = self.ttf_face.descender() as f64 * scale;

        if text.is_empty() {
            return ShapedText {
                path: Path::new(),
                advance_width: 0.0,
                ascent,
                descent,
            };
        }

        // Shape with rustybuzz.
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        let output = rustybuzz::shape(&self.buzz_face, &[], buffer);

        let infos = output.glyph_infos();
        let positions = output.glyph_positions();

        let mut path = Path::new();
        let mut cursor_x = 0.0_f64;
        let mut cursor_y = 0.0_f64;

        for (info, pos) in infos.iter().zip(positions.iter()) {
            let glyph_id = ttf_parser::GlyphId(info.glyph_id as u16);

            // Position offset for this glyph.
            let gx = cursor_x + pos.x_offset as f64 * scale;
            let gy = cursor_y + pos.y_offset as f64 * scale;

            // Extract the outline.
            let mut builder = PathBuilder::new(gx, gy, scale);
            self.ttf_face.outline_glyph(glyph_id, &mut builder);
            builder.flush(&mut path);

            // Advance the cursor.
            cursor_x += pos.x_advance as f64 * scale;
            cursor_y += pos.y_advance as f64 * scale;
        }

        ShapedText {
            path,
            advance_width: cursor_x,
            ascent,
            descent,
        }
    }
}

/// Cursor position information for interactive text editing.
pub struct CursorPositions {
    /// X position for each cursor location (0..=char_count).
    /// Index 0 is before the first character, index N is after the last.
    pub x_positions: Vec<f64>,
}

impl CursorPositions {
    /// Find the closest cursor index for a given local X coordinate.
    pub fn hit_test(&self, local_x: f64) -> usize {
        if self.x_positions.is_empty() {
            return 0;
        }
        let mut best = 0;
        let mut best_dist = f64::MAX;
        for (i, &x) in self.x_positions.iter().enumerate() {
            let dist = (x - local_x).abs();
            if dist < best_dist {
                best_dist = dist;
                best = i;
            }
        }
        best
    }
}

/// The result of shaping a piece of text.
pub struct ShapedText {
    /// Glyph outlines as a single path, baseline at y=0.
    pub path: Path,
    /// Total horizontal advance width.
    pub advance_width: f64,
    /// Ascent above the baseline (positive value going up, but note that in
    /// our coordinate system y increases downward, so this will be negative
    /// in screen coordinates when the text origin is at baseline).
    pub ascent: f64,
    /// Descent below the baseline (negative value in font units, positive
    /// in screen-down coordinates).
    pub descent: f64,
}

impl ShapedText {
    /// Bounding box of the shaped glyph outlines.
    pub fn bounds(&self) -> Bounds {
        self.path.bounding_box()
    }
}

// ── OutlineBuilder adapter ──────────────────────────────────────────

/// Converts ttf-parser's `OutlineBuilder` callbacks into our `SubPath`/`Segment` types.
///
/// Font glyphs use an upward Y axis (ascent is positive Y), but our coordinate
/// system has Y pointing down (standard screen coords). We negate Y during
/// conversion so text renders right-side-up.
struct PathBuilder {
    /// Offset applied to every point (glyph position within the line).
    ox: f64,
    oy: f64,
    /// Scale from font units to user units.
    scale: f64,

    /// Subpaths accumulated so far.
    subpaths: Vec<SubPath>,
    /// The subpath currently being constructed.
    current: Option<SubPath>,
}

impl PathBuilder {
    fn new(ox: f64, oy: f64, scale: f64) -> Self {
        PathBuilder {
            ox,
            oy,
            scale,
            subpaths: Vec::new(),
            current: None,
        }
    }

    /// Map a font-unit coordinate to our coordinate system.
    fn pt(&self, x: f32, y: f32) -> Point {
        Point::new(
            self.ox + x as f64 * self.scale,
            self.oy - y as f64 * self.scale, // flip Y
        )
    }

    /// Flush accumulated subpaths into the target path.
    fn flush(mut self, path: &mut Path) {
        if let Some(sp) = self.current.take()
            && !sp.segments.is_empty()
        {
            self.subpaths.push(sp);
        }
        path.subpaths.extend(self.subpaths);
    }
}

impl ttf_parser::OutlineBuilder for PathBuilder {
    fn move_to(&mut self, x: f32, y: f32) {
        // Close any previous subpath.
        if let Some(sp) = self.current.take()
            && !sp.segments.is_empty()
        {
            self.subpaths.push(sp);
        }
        self.current = Some(SubPath::new(self.pt(x, y)));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.pt(x, y);
        if let Some(ref mut sp) = self.current {
            sp.push_segment(Segment::Line { to: p }, VertexMode::Corner);
        }
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let c = self.pt(x1, y1);
        let p = self.pt(x, y);
        if let Some(ref mut sp) = self.current {
            sp.push_segment(Segment::Quad { ctrl: c, to: p }, VertexMode::Corner);
        }
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let c1 = self.pt(x1, y1);
        let c2 = self.pt(x2, y2);
        let p = self.pt(x, y);
        if let Some(ref mut sp) = self.current {
            sp.push_segment(
                Segment::Cubic {
                    ctrl1: c1,
                    ctrl2: c2,
                    to: p,
                },
                VertexMode::Corner,
            );
        }
    }

    fn close(&mut self) {
        if let Some(mut sp) = self.current.take() {
            sp.closed = true;
            self.subpaths.push(sp);
            self.current = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_font_loads() {
        let font = Font::default_font();
        assert!(font.units_per_em > 0.0);
    }

    #[test]
    fn shape_hello_produces_paths() {
        let font = Font::default_font();
        let shaped = font.shape_text("Hello", 24.0);
        assert!(
            !shaped.path.is_empty(),
            "shaped text should produce glyph outlines"
        );
        assert!(
            shaped.advance_width > 0.0,
            "advance width should be positive"
        );
    }

    #[test]
    fn empty_string_produces_empty_path() {
        let font = Font::default_font();
        let shaped = font.shape_text("", 24.0);
        assert!(shaped.path.is_empty());
        assert_eq!(shaped.advance_width, 0.0);
    }

    #[test]
    fn different_sizes_scale_proportionally() {
        let font = Font::default_font();
        let small = font.shape_text("A", 12.0);
        let large = font.shape_text("A", 24.0);
        let ratio = large.advance_width / small.advance_width;
        assert!(
            (ratio - 2.0).abs() < 0.01,
            "width ratio {} should be ~2.0",
            ratio
        );
    }

    #[test]
    fn glyph_outlines_have_correct_winding() {
        let font = Font::default_font();
        let shaped = font.shape_text("A", 100.0);
        let bounds = shaped.path.bounding_box();
        assert!(
            bounds.min.y < 0.0,
            "glyph tops should be above baseline (negative Y), got min_y={}",
            bounds.min.y
        );
    }

    #[test]
    fn kerning_affects_width() {
        let font = Font::default_font();
        let a = font.shape_text("A", 100.0);
        let v = font.shape_text("V", 100.0);
        let av = font.shape_text("AV", 100.0);
        let sum = a.advance_width + v.advance_width;
        assert!(
            av.advance_width <= sum + 0.1,
            "AV ({}) should not be wider than A+V ({})",
            av.advance_width,
            sum
        );
    }

    #[test]
    fn font_db_resolves_fallback() {
        let mut fdb = FontDb::new();
        // A nonsense family name should resolve to the fallback.
        let font = fdb.resolve("NonExistentFont12345");
        let shaped = font.shape_text("X", 100.0);
        assert!(
            !shaped.path.is_empty(),
            "fallback font should produce glyphs"
        );
    }

    #[test]
    fn font_db_resolves_system_font() {
        let mut fdb = FontDb::new();
        // Liberation Sans is bundled, so it should always resolve.
        let font = fdb.resolve("Liberation Sans");
        let shaped = font.shape_text("Hello", 24.0);
        assert!(
            !shaped.path.is_empty(),
            "Liberation Sans should produce glyph outlines"
        );
    }

    #[test]
    fn font_db_generic_sans_serif() {
        let mut fdb = FontDb::new();
        let font = fdb.resolve("sans-serif");
        let shaped = font.shape_text("Test", 24.0);
        assert!(
            !shaped.path.is_empty(),
            "generic sans-serif should resolve to a font"
        );
    }

    #[test]
    fn global_font_db_works() {
        let shaped = shape_text("Hello", "sans-serif", 24.0);
        assert!(!shaped.path.is_empty());
    }

    #[test]
    fn text_bounds_with_family() {
        let bounds = text_bounds("Hello", "Liberation Sans", 24.0);
        assert!(!bounds.is_empty());
    }
}
