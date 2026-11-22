//! Keyboard navigation helpers: edit points and selection extension.

use kerf_core::{Frame, Project};

/// Places worth jumping to: start, end, clip edges, markers and `extra`
/// (e.g. selection edges). Sorted, without duplicates.
pub fn edit_points(p: &Project, extra: &[Frame]) -> Vec<Frame> {
    let mut v = vec![0, p.length()];
    for t in &p.tracks {
        for c in &t.clips {
            v.push(c.position);
            v.push(c.end());
        }
    }
    v.extend(p.markers.iter().map(|m| m.position));
    v.extend_from_slice(extra);
    v.sort_unstable();
    v.dedup();
    v
}

pub fn prev_point(points: &[Frame], from: Frame) -> Option<Frame> {
    points.iter().rev().copied().find(|&x| x < from)
}

pub fn next_point(points: &[Frame], from: Frame) -> Option<Frame> {
    points.iter().copied().find(|&x| x > from)
}

/// Move the "moving" edge of a selection to `to`. The cursor is the moving
/// edge; the other edge (or the cursor, with no selection) stays put.
/// Returns the new range (`None` if empty) and the new cursor.
pub fn extend(
    range: Option<(Frame, Frame)>,
    cursor: Frame,
    to: Frame,
) -> (Option<(Frame, Frame)>, Frame) {
    let anchor = match range {
        Some((a, b)) if cursor == a => b,
        Some((a, _)) => a,
        None => cursor,
    };
    let (a, b) = (anchor.min(to), anchor.max(to));
    ((b > a).then_some((a, b)), to)
}

/// Arrow-key step: a few pixels at the current zoom, at least one sample.
pub fn nudge_step(samples_per_pixel: f64) -> Frame {
    (4.0 * samples_per_pixel).round().max(1.0) as Frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use kerf_core::AudioBuffer;

    #[test]
    fn edit_points_and_neighbours() {
        let mut p = Project::new(1000, 1);
        p.add_source_as_track("a", None, AudioBuffer::new(1000, vec![vec![0.0; 100]]), 50);
        p.add_marker(120, "m");
        let pts = edit_points(&p, &[70]);
        assert_eq!(pts, [0, 50, 70, 120, 150]);
        assert_eq!(prev_point(&pts, 70), Some(50));
        assert_eq!(next_point(&pts, 70), Some(120));
        assert_eq!(prev_point(&pts, 0), None);
        assert_eq!(next_point(&pts, 150), None);
    }

    #[test]
    fn extending_moves_only_the_cursor_edge() {
        // Start a selection from the cursor.
        let (r, c) = extend(None, 100, 140);
        assert_eq!((r, c), (Some((100, 140)), 140));
        // Keep extending the same edge, then cross back over the anchor.
        let (r, c) = extend(r, c, 160);
        assert_eq!((r, c), (Some((100, 160)), 160));
        let (r, c) = extend(r, c, 80);
        assert_eq!((r, c), (Some((80, 100)), 80));
        // Cursor on the start edge: the end stays put.
        let (r, c) = extend(Some((80, 100)), 80, 90);
        assert_eq!((r, c), (Some((90, 100)), 90));
        // Collapsing to nothing clears the range.
        assert_eq!(extend(Some((90, 100)), 90, 100).0, None);
    }

    #[test]
    fn nudge_is_zoom_relative() {
        assert_eq!(nudge_step(0.1), 1);
        assert_eq!(nudge_step(512.0), 2048);
    }
}
