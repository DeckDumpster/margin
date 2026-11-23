//! Splitting document text into numbered lines.

/// Split on `\n`, drop one trailing `\r` per line; a final newline makes no extra line.
pub fn split(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::split;
    use margin_test_id::test_id;

    #[test_id(t0hoqpa)]
    #[test]
    fn splits_on_newline() {
        assert_eq!(split("a\nb\nc"), ["a", "b", "c"]);
    }

    #[test_id(tdc1rmm)]
    #[test]
    fn crlf_loses_its_carriage_return() {
        assert_eq!(split("a\r\nb\r\n"), ["a", "b"]);
    }

    #[test_id(tgwd3uj)]
    #[test]
    fn final_newline_makes_no_extra_line() {
        assert_eq!(split("a\nb\n"), ["a", "b"]);
    }

    #[test_id(txx6f8h)]
    #[test]
    fn empty_text_has_no_lines() {
        assert!(split("").is_empty());
    }

    #[test_id(tkap1fy)]
    #[test]
    fn blank_lines_are_kept() {
        assert_eq!(split("a\n\n\nb\n"), ["a", "", "", "b"]);
        assert_eq!(split("\n"), [""]);
    }
}
