//! Pull an SVG document out of an LLM response.
//!
//! Models in the wild ignore "no markdown fences" instructions plenty of
//! the time. This extractor is deliberately forgiving: it strips fences,
//! tolerates leading prose, and then locates the `<svg ... </svg>` window.
//! Returns the raw SVG slice (with any preceding `<?xml ?>` prolog kept
//! so usvg can read it).
//!
//! Pure string ops — no regex, no XML parser. We're not trying to be
//! correct in the face of weird tag nesting; we just need to crop down to
//! something usvg can chew on. If usvg rejects the result, the dialog
//! surfaces the parse error and the user can ask for a retry in plain
//! language.

/// Extract an SVG document from arbitrary text. Returns `None` if no
/// `<svg` opening tag is found.
///
/// We deliberately drop any `<?xml ?>` prolog the model emits — usvg
/// parses fine without it, and skipping the prolog frees us from having
/// to reason about prose sitting between the prolog and `<svg`.
pub fn extract_svg(text: &str) -> Option<String> {
    let stripped = strip_code_fence(text.trim());

    // Find the opening "<svg" — case-insensitive only for the tag name
    // itself; SVG itself is XML and case-sensitive, but we've seen models
    // produce "<SVG" once or twice, so be lenient.
    let svg_open = find_ci(stripped, "<svg")?;

    // Match closing `</svg>`. Models occasionally truncate before the
    // close tag; in that case we still return what we have so the caller
    // can show a parse error rather than a silent failure.
    let after_open = &stripped[svg_open..];
    let end_rel = find_ci(after_open, "</svg>")
        .map(|i| i + "</svg>".len())
        .unwrap_or(after_open.len());

    Some(after_open[..end_rel].to_string())
}

/// Strip a triple-backtick code fence around `text`, if present. Handles
/// the common variants ```` ```svg ````, ```` ```xml ````, plain
/// ```` ``` ````. If no fence wraps the entire string, returns it
/// unchanged.
fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    // Skip an optional language tag on the first line.
    let after_lang = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => rest,
    };
    // Trim trailing fence — accept either "```" at very end or trailing
    // newline followed by "```".
    let trimmed = after_lang.trim_end();
    trimmed.strip_suffix("```").unwrap_or(after_lang).trim_end()
}

/// Case-insensitive substring search returning a byte offset into
/// `haystack`. We only need ASCII case-folding (the patterns are tag
/// fragments), so this stays cheap.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let nb = needle.as_bytes();
    let hb = haystack.as_bytes();
    if nb.is_empty() || hb.len() < nb.len() {
        return None;
    }
    'outer: for i in 0..=hb.len() - nb.len() {
        for j in 0..nb.len() {
            if !hb[i + j].eq_ignore_ascii_case(&nb[j]) {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN_SVG: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"><circle cx="5" cy="5" r="4"/></svg>"#;

    #[test]
    fn passes_through_clean_svg() {
        let out = extract_svg(MIN_SVG).unwrap();
        assert_eq!(out, MIN_SVG);
    }

    #[test]
    fn strips_markdown_fences() {
        let wrapped = format!("```svg\n{MIN_SVG}\n```");
        let out = extract_svg(&wrapped).unwrap();
        assert_eq!(out, MIN_SVG);
    }

    #[test]
    fn strips_xml_fence() {
        let wrapped = format!("```xml\n{MIN_SVG}\n```");
        let out = extract_svg(&wrapped).unwrap();
        assert_eq!(out, MIN_SVG);
    }

    #[test]
    fn strips_bare_fence() {
        let wrapped = format!("```\n{MIN_SVG}\n```");
        let out = extract_svg(&wrapped).unwrap();
        assert_eq!(out, MIN_SVG);
    }

    #[test]
    fn ignores_leading_prose() {
        let chatty = format!("Sure, here you go!\n{MIN_SVG}\nLet me know if…");
        let out = extract_svg(&chatty).unwrap();
        // Trailing prose is dropped because we cut at `</svg>`.
        assert!(out.starts_with("<svg"));
        assert!(out.ends_with("</svg>"));
    }

    #[test]
    fn returns_none_on_garbage() {
        assert!(extract_svg("no svg here, sorry").is_none());
        assert!(extract_svg("").is_none());
    }

    #[test]
    fn tolerates_uppercase_svg_tag() {
        let weird = r#"<SVG viewBox="0 0 1 1"><rect/></SVG>"#;
        let out = extract_svg(weird).unwrap();
        assert_eq!(out, weird);
    }

    #[test]
    fn keeps_truncated_svg_for_diagnostics() {
        let truncated = r#"<svg viewBox="0 0 1 1"><rect"#;
        let out = extract_svg(truncated).unwrap();
        // No </svg>: we return everything from <svg to end of text so the
        // caller can show a parse error rather than silently failing.
        assert_eq!(out, truncated);
    }
}
