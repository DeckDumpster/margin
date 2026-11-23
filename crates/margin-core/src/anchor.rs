//! Where a comment hangs: a first and last line of one document version, and optionally
//! the columns of a highlight that may run across lines.

/// Lines are 1-based. Columns count Unicode scalar values: `cols.0` is the start on
/// `first`, `cols.1` the exclusive end on `last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    pub first: u32,
    pub last: u32,
    pub cols: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorError {
    /// `first` is after `last`.
    LinesReversed,
    /// A line is 0 or past the end of the document.
    LineOutOfRange,
    /// A column is past the end of its line.
    ColumnOutOfRange,
    /// On a single line the start is not before the end.
    ColumnsReversed,
}

impl std::fmt::Display for AnchorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AnchorError::LinesReversed => "first line is after last line",
            AnchorError::LineOutOfRange => "line is outside the document",
            AnchorError::ColumnOutOfRange => "column is past the end of its line",
            AnchorError::ColumnsReversed => "start column is not before end column",
        })
    }
}

impl std::error::Error for AnchorError {}

impl Anchor {
    pub fn lines(first: u32, last: u32) -> Anchor {
        Anchor {
            first,
            last,
            cols: None,
        }
    }

    /// `line_lens[i]` is the length in scalar values of line `i + 1`.
    pub fn validate(&self, line_lens: &[u32]) -> Result<(), AnchorError> {
        if self.first > self.last {
            return Err(AnchorError::LinesReversed);
        }
        let count = line_lens.len() as u32;
        if self.first == 0 || self.last > count {
            return Err(AnchorError::LineOutOfRange);
        }
        if let Some((start, end)) = self.cols {
            if start > line_lens[self.first as usize - 1] || end > line_lens[self.last as usize - 1]
            {
                return Err(AnchorError::ColumnOutOfRange);
            }
            if self.first == self.last && start >= end {
                return Err(AnchorError::ColumnsReversed);
            }
        }
        Ok(())
    }

    pub fn covers(&self, line: u32) -> bool {
        self.first <= line && line <= self.last
    }

    pub fn overlaps(&self, other: &Anchor) -> bool {
        self.first <= other.last && other.first <= self.last
    }
}

/// A lane for each anchor, in input order. Anchors are placed in `first` order (ties by
/// `last`, then input order) on the lowest lane whose previous anchor has ended, so
/// overlapping anchors never share a lane and a lane is reused once it is free.
pub fn lanes(anchors: &[Anchor]) -> Vec<u16> {
    let mut order: Vec<usize> = (0..anchors.len()).collect();
    order.sort_by_key(|&i| (anchors[i].first, anchors[i].last, i));
    let mut lane_end: Vec<u32> = Vec::new();
    let mut out = vec![0u16; anchors.len()];
    for i in order {
        let a = anchors[i];
        let lane = match lane_end.iter().position(|&end| end < a.first) {
            Some(l) => l,
            None => {
                lane_end.push(0);
                lane_end.len() - 1
            }
        };
        lane_end[lane] = a.last;
        out[i] = lane as u16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_test_id::test_id;

    #[test_id(t6ecvpp)]
    #[test]
    fn two_overlapping_ranges_get_different_lanes() {
        let l = lanes(&[Anchor::lines(1, 10), Anchor::lines(5, 15)]);
        assert_eq!(l, [0, 1]);
    }

    #[test_id(tla10am)]
    #[test]
    fn a_single_line_inside_a_range_gets_a_third_lane() {
        let l = lanes(&[
            Anchor::lines(1, 10),
            Anchor::lines(5, 15),
            Anchor::lines(7, 7),
        ]);
        assert_eq!(l, [0, 1, 2]);
    }

    #[test_id(thl9xkg)]
    #[test]
    fn a_disjoint_later_anchor_reuses_lane_zero() {
        let l = lanes(&[
            Anchor::lines(1, 10),
            Anchor::lines(5, 15),
            Anchor::lines(20, 25),
        ]);
        assert_eq!(l, [0, 1, 0]);
    }

    #[test_id(tnrpqtb)]
    #[test]
    fn lanes_follow_input_order_not_first_order() {
        let l = lanes(&[Anchor::lines(5, 15), Anchor::lines(1, 10)]);
        assert_eq!(l, [1, 0]);
    }

    #[test_id(t1f5lbz)]
    #[test]
    fn touching_anchors_on_a_shared_line_do_not_share_a_lane() {
        let l = lanes(&[Anchor::lines(1, 3), Anchor::lines(3, 5)]);
        assert_eq!(l, [0, 1]);
    }

    #[test_id(thtwdpt)]
    #[test]
    fn covers_and_overlaps_are_inclusive_of_both_ends() {
        let a = Anchor::lines(3, 6);
        assert!(a.covers(3) && a.covers(6) && !a.covers(2) && !a.covers(7));
        assert!(a.overlaps(&Anchor::lines(6, 9)));
        assert!(!a.overlaps(&Anchor::lines(7, 9)));
    }

    #[test_id(td3ki67)]
    #[test]
    fn validate_refuses_each_bad_shape() {
        let lens = [5, 0, 8];
        let cols = |first, last, c| Anchor {
            first,
            last,
            cols: Some(c),
        };
        assert_eq!(
            Anchor::lines(2, 1).validate(&lens),
            Err(AnchorError::LinesReversed)
        );
        assert_eq!(
            Anchor::lines(0, 1).validate(&lens),
            Err(AnchorError::LineOutOfRange)
        );
        assert_eq!(
            Anchor::lines(3, 4).validate(&lens),
            Err(AnchorError::LineOutOfRange)
        );
        assert_eq!(
            cols(1, 1, (0, 6)).validate(&lens),
            Err(AnchorError::ColumnOutOfRange)
        );
        assert_eq!(
            cols(1, 3, (6, 2)).validate(&lens),
            Err(AnchorError::ColumnOutOfRange)
        );
        assert_eq!(
            cols(1, 1, (3, 2)).validate(&lens),
            Err(AnchorError::ColumnsReversed)
        );
        assert_eq!(Anchor::lines(1, 3).validate(&lens), Ok(()));
        assert_eq!(cols(1, 3, (4, 2)).validate(&lens), Ok(()));
        assert_eq!(cols(1, 1, (1, 5)).validate(&lens), Ok(()));
    }
}
