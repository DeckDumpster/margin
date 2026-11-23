//! Cutting one line into runs of highlighted columns, so overlapping highlights each show.

use std::ops::Range;

use crate::anchor::Anchor;

/// A run of columns (Unicode scalar values) on one line and the lanes of every highlight
/// covering it, ascending.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub range: Range<u32>,
    pub lanes: Vec<u16>,
}

/// The columns of `line` (1-based, `len` scalar values long) an anchor highlights, or `None`
/// when it has no columns or does not reach the line. A cross-line anchor gives `start..len`
/// on its first line, the whole of each middle line and `0..end` on its last.
pub fn span_on_line(anchor: &Anchor, line: u32, len: u32) -> Option<(u32, u32)> {
    let (start, end) = anchor.cols?;
    if !anchor.covers(line) {
        return None;
    }
    let from = if line == anchor.first { start } else { 0 };
    let to = if line == anchor.last { end } else { len };
    (from < to).then_some((from, to))
}

/// The runs of one line covered by at least one of `spans` (`(lane, start, end)`, end
/// exclusive), in column order; a boundary falls wherever the covering set changes, and
/// neighbouring runs with the same set are one run. Columns past `line_len` are cut off.
pub fn segments(line_len: u32, spans: &[(u16, u32, u32)]) -> Vec<Segment> {
    let mut cuts: Vec<u32> = spans
        .iter()
        .flat_map(|&(_, s, e)| [s.min(line_len), e.min(line_len)])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out: Vec<Segment> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mut lanes: Vec<u16> = spans
            .iter()
            .filter(|&&(_, s, e)| s <= a && b <= e)
            .map(|&(l, _, _)| l)
            .collect();
        if lanes.is_empty() {
            continue;
        }
        lanes.sort_unstable();
        lanes.dedup();
        match out.last_mut() {
            Some(prev) if prev.range.end == a && prev.lanes == lanes => prev.range.end = b,
            _ => out.push(Segment { range: a..b, lanes }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_test_id::test_id;

    fn seg(range: Range<u32>, lanes: &[u16]) -> Segment {
        Segment {
            range,
            lanes: lanes.to_vec(),
        }
    }

    fn anchor(first: u32, last: u32, cols: (u32, u32)) -> Anchor {
        Anchor {
            first,
            last,
            cols: Some(cols),
        }
    }

    #[test_id(t9ji6u9)]
    #[test]
    fn a_highlight_inside_one_line_is_one_run() {
        let a = anchor(4, 4, (3, 9));
        assert_eq!(span_on_line(&a, 4, 20), Some((3, 9)));
        assert_eq!(span_on_line(&a, 3, 20), None);
        assert_eq!(segments(20, &[(0, 3, 9)]), [seg(3..9, &[0])]);
    }

    #[test_id(tix10ad)]
    #[test]
    fn a_highlight_across_three_lines_covers_the_middle_line_whole() {
        let a = anchor(2, 4, (5, 3));
        assert_eq!(span_on_line(&a, 2, 12), Some((5, 12)));
        assert_eq!(span_on_line(&a, 3, 8), Some((0, 8)));
        assert_eq!(span_on_line(&a, 4, 10), Some((0, 3)));
        assert_eq!(span_on_line(&a, 5, 10), None);
        assert_eq!(span_on_line(&Anchor::lines(2, 4), 3, 8), None);
    }

    #[test_id(ty5meom)]
    #[test]
    fn two_overlapping_highlights_give_the_shared_words_both_lanes() {
        let got = segments(30, &[(0, 2, 10), (1, 6, 14)]);
        assert_eq!(
            got,
            [seg(2..6, &[0]), seg(6..10, &[0, 1]), seg(10..14, &[1])]
        );
    }

    #[test_id(tp9fe2b)]
    #[test]
    fn touching_highlights_of_one_lane_merge_and_empty_ones_vanish() {
        assert_eq!(
            segments(30, &[(0, 2, 5), (0, 5, 8), (1, 9, 9)]),
            [seg(2..8, &[0])]
        );
        assert_eq!(segments(30, &[]), []);
    }

    #[test_id(ty24gdz)]
    #[test]
    fn columns_count_an_emoji_and_a_tab_as_one_each() {
        // "\t🙂x": the tab is column 0, the emoji column 1, the x column 2.
        let line = "\t🙂x";
        let len = line.chars().count() as u32;
        assert_eq!(len, 3);
        assert_eq!(segments(len, &[(0, 1, 2)]), [seg(1..2, &[0])]);
        let picked: String = line.chars().skip(1).take(1).collect();
        assert_eq!(picked, "🙂");
        // Past the end is cut to the line.
        assert_eq!(segments(len, &[(0, 2, 99)]), [seg(2..3, &[0])]);
    }
}
