//! Carrying an anchor from one version of a document to the next by matching its lines' text.

use similar::{Algorithm, DiffOp, capture_diff_slices};

use crate::anchor::Anchor;

/// Where an anchor lands on the new version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carried {
    /// The anchor's surviving lines, moved; a highlight keeps its columns.
    Placed(Anchor),
    /// No line of the range survives; `at` is the nearest surviving line (new numbering).
    Removed { at: u32 },
    /// A highlight whose first or last line was edited: its surviving line span, no columns.
    TextChanged(Anchor),
}

/// For each old line (index `i` is line `i + 1`), the new line number it is unchanged at.
pub fn line_map(old: &[&str], new: &[&str]) -> Vec<Option<u32>> {
    let mut map = vec![None; old.len()];
    for op in capture_diff_slices(Algorithm::Patience, old, new) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for k in 0..len {
                map[old_index + k] = Some((new_index + k + 1) as u32);
            }
        }
    }
    map
}

/// Where `anchor`, written against `old`, sits on `new`, given `map = line_map(old, new)`.
pub fn carry(anchor: &Anchor, map: &[Option<u32>], old: &[&str], new: &[&str]) -> Carried {
    let at = |line: u32| map.get(line as usize - 1).copied().flatten();
    let range = anchor.first..=anchor.last;
    let first = range.clone().find_map(at);
    let last = range.clone().rev().find_map(at);
    let (Some(first), Some(last)) = (first, last) else {
        let before = (1..anchor.first).rev().find_map(at);
        let after = (anchor.last + 1..=map.len() as u32).find_map(at);
        if anchor.cols.is_some() {
            // A highlight whose lines were rewritten, not deleted: the new text between its
            // neighbours is what its words became, so it falls back to those lines.
            let from = before.unwrap_or(0) + 1;
            let to = after.map_or(new.len() as u32, |q| q - 1);
            if from <= to {
                let len = anchor.last - anchor.first;
                return Carried::TextChanged(Anchor::lines(from, to.min(from + len)));
            }
        }
        return Carried::Removed {
            at: before.or(after).unwrap_or(1),
        };
    };
    let span = Anchor::lines(first, last);
    let Some((start, end)) = anchor.cols else {
        return Carried::Placed(span);
    };
    let width = |lines: &[&str], line: u32| lines[line as usize - 1].chars().count() as u32;
    let verbatim = at(anchor.first).is_some()
        && at(anchor.last).is_some()
        && start <= width(old, anchor.first)
        && end <= width(old, anchor.last);
    if verbatim {
        Carried::Placed(Anchor {
            cols: Some((start, end)),
            ..span
        })
    } else {
        Carried::TextChanged(span)
    }
}

/// The new lines no old line maps to: inserted or edited.
pub fn changed_lines(map: &[Option<u32>], new_len: usize) -> Vec<u32> {
    let mut kept = vec![false; new_len];
    for n in map.iter().flatten() {
        kept[*n as usize - 1] = true;
    }
    (1..=new_len as u32)
        .filter(|n| !kept[*n as usize - 1])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_test_id::test_id;

    fn lines(s: &str) -> Vec<&str> {
        crate::lines::split(s)
    }

    fn run(old: &str, new: &str, a: Anchor) -> Carried {
        let (o, n) = (lines(old), lines(new));
        carry(&a, &line_map(&o, &n), &o, &n)
    }

    #[test_id(tswxj0g)]
    #[test]
    fn a_line_moved_down_by_inserted_lines_above() {
        let c = run("a\nb\nc\nd", "x\ny\na\nb\nc\nd", Anchor::lines(3, 3));
        assert_eq!(c, Carried::Placed(Anchor::lines(5, 5)));
    }

    #[test_id(tb5sj9w)]
    #[test]
    fn a_range_with_its_middle_line_deleted_keeps_its_ends() {
        let c = run("a\nb\nc\nd\ne", "a\nb\nd\ne", Anchor::lines(2, 4));
        assert_eq!(c, Carried::Placed(Anchor::lines(2, 3)));
        let c = run("a\nb\nc\nd\ne", "a\nb\nX\nd\ne", Anchor::lines(2, 4));
        assert_eq!(c, Carried::Placed(Anchor::lines(2, 4)));
    }

    #[test_id(tm2fl2v)]
    #[test]
    fn a_range_wholly_deleted_sits_at_the_nearest_earlier_surviving_line() {
        let c = run("a\nb\nc\nd\ne", "a\nb\ne", Anchor::lines(3, 4));
        assert_eq!(c, Carried::Removed { at: 2 });
    }

    #[test_id(tdzjsdu)]
    #[test]
    fn a_range_deleted_at_the_top_sits_at_the_nearest_later_line() {
        let c = run("a\nb\nc\nd", "c\nd", Anchor::lines(1, 2));
        assert_eq!(c, Carried::Removed { at: 1 });
        let c = run("a\nb\nc\nd", "X\nc\nd", Anchor::lines(1, 2));
        assert_eq!(c, Carried::Removed { at: 2 });
    }

    #[test_id(t9p7did)]
    #[test]
    fn a_document_emptied_leaves_the_thread_at_line_one() {
        assert_eq!(
            run("a\nb", "", Anchor::lines(1, 2)),
            Carried::Removed { at: 1 }
        );
    }

    #[test_id(twwcpju)]
    #[test]
    fn a_highlight_whose_line_is_unchanged_keeps_its_columns() {
        let a = Anchor {
            first: 2,
            last: 2,
            cols: Some((1, 3)),
        };
        let c = run("a\nhello\nc", "z\na\nhello\nc", a);
        assert_eq!(
            c,
            Carried::Placed(Anchor {
                first: 3,
                last: 3,
                cols: Some((1, 3))
            })
        );
    }

    #[test_id(tbh5bsg)]
    #[test]
    fn a_highlight_whose_line_was_edited_is_text_changed() {
        let a = Anchor {
            first: 2,
            last: 2,
            cols: Some((1, 3)),
        };
        let c = run("a\nhello\nc", "a\nhelp\nc", a);
        assert_eq!(c, Carried::TextChanged(Anchor::lines(2, 2)));
        let multi = Anchor {
            first: 1,
            last: 3,
            cols: Some((0, 1)),
        };
        let c = run("a\nb\nc", "a\nb\nC", multi);
        assert_eq!(c, Carried::TextChanged(Anchor::lines(1, 2)));
    }

    #[test_id(tqe92v8)]
    #[test]
    fn a_rewritten_highlight_falls_back_to_the_new_lines_between_its_neighbours() {
        let hl = |first, last| Anchor {
            first,
            last,
            cols: Some((0, 1)),
        };
        // Three old lines rewritten as five: the fallback is as long as the old range.
        let c = run("a\nb\nc\nd\ne", "a\n1\n2\n3\n4\ne", hl(2, 4));
        assert_eq!(c, Carried::TextChanged(Anchor::lines(2, 4)));
        // Three old lines rewritten as one: the fallback is that one line.
        let c = run("a\nb\nc\nd\ne", "a\n1\ne", hl(2, 4));
        assert_eq!(c, Carried::TextChanged(Anchor::lines(2, 2)));
        // Deleted outright, no new text between its neighbours: the thread is removed.
        let c = run("a\nb\nc", "a\nc", hl(2, 2));
        assert_eq!(c, Carried::Removed { at: 1 });
    }

    #[test_id(tbz9reb)]
    #[test]
    fn duplicate_identical_lines_map_in_order() {
        let map = line_map(&["x", "x", "x"], &["x", "x", "x", "x"]);
        assert_eq!(map, [Some(1), Some(2), Some(3)]);
    }

    #[test_id(tb3agqf)]
    #[test]
    fn changed_lines_lists_inserted_and_edited_lines_only() {
        let (o, n) = (lines("a\nb\nc\nd"), lines("a\nNEW\nc\nD\nd"));
        let map = line_map(&o, &n);
        assert_eq!(changed_lines(&map, n.len()), [2, 4]);
    }
}
