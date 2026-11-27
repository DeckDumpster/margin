//! Cutting a review into pages of whole documents.

use std::ops::Range;

/// The documents of each page, as index ranges into `lines`, the documents' line counts. Whole
/// documents are packed in order up to `page_lines` lines; one longer than that has a page to
/// itself. No documents give one empty page.
pub fn split(lines: &[usize], page_lines: usize) -> Vec<Range<usize>> {
    let mut pages = Vec::new();
    let (mut start, mut held) = (0, 0);
    for (i, &n) in lines.iter().enumerate() {
        if i > start && held + n > page_lines {
            pages.push(start..i);
            (start, held) = (i, 0);
        }
        held += n;
    }
    pages.push(start..lines.len());
    pages
}

#[cfg(test)]
mod tests {
    use super::split;
    use margin_test_id::test_id;

    #[test_id(t5g3ozw)]
    #[test]
    fn whole_documents_are_packed_in_order_up_to_the_limit() {
        assert_eq!(split(&[4, 6, 1, 5, 5, 2], 10), [0..2, 2..4, 4..6]);
    }

    #[test_id(txpd1af)]
    #[test]
    fn a_document_over_the_limit_has_a_page_to_itself() {
        assert_eq!(split(&[2, 11, 3], 10), [0..1, 1..2, 2..3]);
        assert_eq!(split(&[11, 11], 10), [0..1, 1..2]);
    }

    #[test_id(tbrdwru)]
    #[test]
    fn every_document_is_on_exactly_one_page() {
        let lines = [0, 7, 30, 0, 10, 1, 9, 0, 10, 10, 11, 0];
        let on: Vec<usize> = split(&lines, 10).into_iter().flatten().collect();
        assert_eq!(on, (0..lines.len()).collect::<Vec<_>>());
    }

    #[test_id(tw7x81z)]
    #[test]
    fn documents_that_fit_together_make_one_page_and_none_make_one_empty_page() {
        assert_eq!(split(&[3, 3, 4], 10), Vec::from_iter(std::iter::once(0..3)));
        assert_eq!(split(&[], 10), Vec::from_iter(std::iter::once(0..0)));
        assert_eq!(split(&[0, 0], 10), Vec::from_iter(std::iter::once(0..2)));
    }
}
