//! Dash pattern expansion: splits a `Path` into visible dash sub-paths.
//!
//! SVG dash semantics:
//! - `stroke-dasharray` is a list of alternating dash/gap lengths.
//! - If the array has an odd number of entries, it is repeated to make it even.
//! - `stroke-dashoffset` shifts the starting position into the pattern.
//! - Each visible dash becomes an open sub-path in the output.

use vector_geom::{Path, SubPath};

/// A dash pattern: alternating dash/gap lengths and a starting offset.
#[derive(Debug, Clone)]
pub struct DashPattern {
    pub array: Vec<f64>,
    pub offset: f64,
}

/// Expand a path according to a dash pattern, returning a new path containing
/// only the visible (drawn) portions as individual open sub-paths.
pub fn dash_path(path: &Path, pattern: &DashPattern) -> Path {
    if pattern.array.is_empty() {
        return path.clone();
    }

    // Normalize: if odd-length, double it per SVG spec.
    let array: Vec<f64> = if !pattern.array.len().is_multiple_of(2) {
        pattern
            .array
            .iter()
            .chain(pattern.array.iter())
            .copied()
            .collect()
    } else {
        pattern.array.clone()
    };

    // Total pattern length.
    let pattern_len: f64 = array.iter().sum();
    if pattern_len <= 0.0 {
        return path.clone();
    }

    let mut result = Path::new();

    for subpath in &path.subpaths {
        dash_subpath(subpath, &array, pattern.offset, pattern_len, &mut result);
    }

    result
}

/// Flush a completed dash sub-path to output if it has content.
fn flush_dash(dash: &mut Option<SubPath>, out: &mut Path) {
    if let Some(d) = dash.take()
        && !d.segments.is_empty()
    {
        out.subpaths.push(d);
    }
}

/// Advance to the next dash/gap element, updating state accordingly.
fn advance_element(
    dash_idx: &mut usize,
    remaining: &mut f64,
    drawing: &mut bool,
    current_dash: &mut Option<SubPath>,
    out: &mut Path,
    array: &[f64],
    new_start: vector_geom::Point,
) {
    if *drawing {
        flush_dash(current_dash, out);
    }
    *dash_idx = (*dash_idx + 1) % array.len();
    *remaining = array[*dash_idx];
    *drawing = dash_idx.is_multiple_of(2);
    *current_dash = if *drawing {
        Some(SubPath::new(new_start))
    } else {
        None
    };
}

/// Expand dashes for a single subpath.
fn dash_subpath(subpath: &SubPath, array: &[f64], offset: f64, pattern_len: f64, out: &mut Path) {
    if subpath.segments.is_empty() {
        return;
    }

    // Compute starting position in the dash pattern (accounting for offset).
    let mut pos_in_pattern = offset % pattern_len;
    if pos_in_pattern < 0.0 {
        pos_in_pattern += pattern_len;
    }

    // Find which element of the array we start in, and how far into it.
    let mut dash_idx = 0usize;
    let mut remaining_in_element = 0.0;
    {
        let mut acc = 0.0;
        for (i, &len) in array.iter().enumerate() {
            if acc + len > pos_in_pattern {
                dash_idx = i;
                remaining_in_element = len - (pos_in_pattern - acc);
                break;
            }
            acc += len;
        }
    }

    let mut drawing = dash_idx.is_multiple_of(2);
    let mut current_dash: Option<SubPath> = if drawing {
        Some(SubPath::new(subpath.start))
    } else {
        None
    };

    let mut current_point = subpath.start;

    for seg in &subpath.segments {
        let seg_len = seg.arc_length(current_point);
        if seg_len < 1e-12 {
            // Zero-length segment: just append if drawing.
            if let Some(ref mut dash) = current_dash {
                dash.push_segment(*seg, vector_geom::VertexMode::Corner);
            }
            current_point = seg.endpoint();
            continue;
        }

        // Walk through this segment, splitting at dash boundaries.
        let mut seg_distance_consumed = 0.0;
        let mut seg_from = current_point;
        let mut remaining_seg = *seg;

        while seg_distance_consumed < seg_len - 1e-12 {
            let seg_remaining_len = seg_len - seg_distance_consumed;

            if remaining_in_element >= seg_remaining_len - 1e-12 {
                // The rest of this segment fits within the current dash element.
                remaining_in_element -= seg_remaining_len;

                if let Some(ref mut dash) = current_dash {
                    dash.push_segment(remaining_seg, vector_geom::VertexMode::Corner);
                }

                // If we exactly exhausted this dash element, advance.
                if remaining_in_element < 1e-12 {
                    advance_element(
                        &mut dash_idx,
                        &mut remaining_in_element,
                        &mut drawing,
                        &mut current_dash,
                        out,
                        array,
                        remaining_seg.endpoint(),
                    );
                }

                break; // Done with this segment.
            }

            // The dash boundary falls within this segment. Split it.
            let t_split = remaining_in_element / seg_remaining_len;
            let (first, second) = remaining_seg.split_at(seg_from, t_split);

            if let Some(ref mut dash) = current_dash {
                dash.push_segment(first, vector_geom::VertexMode::Corner);
            }

            seg_distance_consumed += remaining_in_element;
            seg_from = first.endpoint();
            remaining_seg = second;

            advance_element(
                &mut dash_idx,
                &mut remaining_in_element,
                &mut drawing,
                &mut current_dash,
                out,
                array,
                seg_from,
            );
        }

        current_point = seg.endpoint();
    }

    // Flush any remaining dash.
    flush_dash(&mut current_dash, out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vector_geom::{Path, Point, Segment, SubPath, VertexMode};

    fn line_path(x0: f64, y0: f64, x1: f64, y1: f64) -> Path {
        Path {
            subpaths: vec![SubPath {
                start: Point::new(x0, y0),
                segments: vec![Segment::Line {
                    to: Point::new(x1, y1),
                }],
                closed: false,
                vertex_modes: vec![VertexMode::Corner; 2],
            }],
        }
    }

    #[test]
    fn solid_when_empty_array() {
        let path = line_path(0.0, 0.0, 100.0, 0.0);
        let pattern = DashPattern {
            array: vec![],
            offset: 0.0,
        };
        let result = dash_path(&path, &pattern);
        assert_eq!(result.subpaths.len(), 1);
    }

    #[test]
    fn simple_dash_pattern() {
        // 100-unit line with dash=10, gap=10 → 5 dashes.
        let path = line_path(0.0, 0.0, 100.0, 0.0);
        let pattern = DashPattern {
            array: vec![10.0, 10.0],
            offset: 0.0,
        };
        let result = dash_path(&path, &pattern);
        assert_eq!(
            result.subpaths.len(),
            5,
            "expected 5 dashes, got {}",
            result.subpaths.len()
        );

        // Each dash should be ~10 units long.
        for (i, sp) in result.subpaths.iter().enumerate() {
            let len = sp.arc_length();
            assert!(
                (len - 10.0).abs() < 0.1,
                "dash {} length {} != 10.0",
                i,
                len
            );
        }
    }

    #[test]
    fn dash_with_offset() {
        // 100-unit line with dash=10, gap=10, offset=5 → starts 5 units into first dash.
        let path = line_path(0.0, 0.0, 100.0, 0.0);
        let pattern = DashPattern {
            array: vec![10.0, 10.0],
            offset: 5.0,
        };
        let result = dash_path(&path, &pattern);

        // First dash should be ~5 units (remainder of first dash element).
        let first_len = result.subpaths[0].arc_length();
        assert!(
            (first_len - 5.0).abs() < 0.1,
            "first dash length {} != 5.0",
            first_len
        );
    }

    #[test]
    fn odd_array_is_doubled() {
        // Odd array [10, 5, 5] becomes [10, 5, 5, 10, 5, 5] (total 40).
        let path = line_path(0.0, 0.0, 40.0, 0.0);
        let pattern = DashPattern {
            array: vec![10.0, 5.0, 5.0],
            offset: 0.0,
        };
        let result = dash_path(&path, &pattern);
        // Pattern after doubling: dash=10, gap=5, dash=5, gap=10, dash=5, gap=5
        // On a 40-unit line: dash(10) gap(5) dash(5) gap(10) dash(5) gap(5)
        // = 3 dashes
        assert_eq!(
            result.subpaths.len(),
            3,
            "expected 3 dashes, got {}",
            result.subpaths.len()
        );
    }

    #[test]
    fn dash_on_cubic_curve() {
        // A cubic bezier with a dash pattern.
        let path = Path {
            subpaths: vec![SubPath {
                start: Point::new(0.0, 0.0),
                segments: vec![Segment::Cubic {
                    ctrl1: Point::new(0.0, 50.0),
                    ctrl2: Point::new(100.0, 50.0),
                    to: Point::new(100.0, 0.0),
                }],
                closed: false,
                vertex_modes: vec![VertexMode::Corner; 2],
            }],
        };
        // Curve is roughly 130-ish units long.
        let pattern = DashPattern {
            array: vec![20.0, 10.0],
            offset: 0.0,
        };
        let result = dash_path(&path, &pattern);
        // Should produce multiple dashes.
        assert!(
            result.subpaths.len() >= 3,
            "expected at least 3 dashes on cubic"
        );

        // Total drawn length should be roughly 2/3 of the curve.
        let total_dash_len: f64 = result.subpaths.iter().map(|sp| sp.arc_length()).sum();
        let curve_len = path.subpaths[0].arc_length();
        let ratio = total_dash_len / curve_len;
        assert!(
            (ratio - 2.0 / 3.0).abs() < 0.1,
            "dash ratio {} should be ~0.667",
            ratio
        );
    }
}
