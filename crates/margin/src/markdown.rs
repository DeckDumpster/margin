//! The cover note's markdown, rendered to HTML that carries nothing the author did not write as
//! markdown: raw HTML stays text, and only a few link schemes survive.

use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, TagEnd, html};

use crate::highlight::escape;
use crate::refs::Refs;

/// Whether a link to `dest` may stay a link.
fn safe_link(dest: &str) -> bool {
    let d = dest.trim().to_ascii_lowercase();
    d.starts_with("http://")
        || d.starts_with("https://")
        || d.starts_with("mailto:")
        || d.starts_with('#')
}

/// `text` as HTML: markdown with tables; raw HTML shown as text; line breaks kept; links only to
/// `http`, `https`, `mailto` or `#…` (any other keeps its text alone); an image as its alt text.
/// A code span that `refs` resolves is linked to what it names, unless the author linked it.
pub fn html(text: &str, refs: &Refs) -> String {
    // One entry per open link: whether it stays a link.
    let mut links: Vec<bool> = Vec::new();
    let mut in_link = 0usize;
    let events = Parser::new_ext(text, Options::ENABLE_TABLES).filter_map(|e| match e {
        Event::Html(h) | Event::InlineHtml(h) => Some(Event::Text(h)),
        Event::SoftBreak => Some(Event::HardBreak),
        Event::Code(code) if in_link == 0 => Some(match refs.resolve(&code) {
            Some(href) => Event::Html(CowStr::from(format!(
                "<a href=\"{}\"><code>{}</code></a>",
                escape(&href),
                escape(&code)
            ))),
            None => Event::Code(code),
        }),
        Event::Start(Tag::Link { ref dest_url, .. }) => {
            in_link += 1;
            let keep = safe_link(dest_url);
            links.push(keep);
            keep.then_some(e)
        }
        Event::End(TagEnd::Link) => {
            in_link = in_link.saturating_sub(1);
            links.pop().unwrap_or(false).then_some(e)
        }
        Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
        e => Some(e),
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use crate::refs::Refs;
    use margin_test_id::test_id;

    fn html(text: &str) -> String {
        super::html(text, &Refs::none())
    }

    #[test_id(twmqa6h)]
    #[test]
    fn headings_lists_code_emphasis_and_links_render() {
        let h = html(
            "## Head\n\n- a\n  - b\n- c\n\nuse `x` and *em* and **strong** [s](https://e.com)\n\n```\nlet a = 1;\n```",
        );
        assert!(h.contains("<h2>Head</h2>"), "{h}");
        assert!(h.contains("<ul>\n<li>a\n<ul>\n<li>b</li>"), "{h}");
        assert!(
            h.contains("<code>x</code>")
                && h.contains("<em>em</em>")
                && h.contains("<strong>strong</strong>"),
            "{h}"
        );
        assert!(h.contains(r#"<a href="https://e.com">s</a>"#), "{h}");
        assert!(h.contains("<pre><code>let a = 1;"), "{h}");
    }

    #[test_id(tyrqac8)]
    #[test]
    fn a_table_renders() {
        let h = html("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(
            h.contains("<table>") && h.contains("<th>a</th>") && h.contains("<td>2</td>"),
            "{h}"
        );
    }

    #[test_id(t5pow8s)]
    #[test]
    fn consecutive_lines_keep_their_breaks() {
        let h = html("Source repository: r\nSource commit: c");
        assert!(h.contains("r<br />\nSource commit"), "{h}");
    }

    #[test_id(tw2pko0)]
    #[test]
    fn raw_html_becomes_text() {
        for src in [
            "<script>alert(1)</script>",
            "a <img src=x onerror=alert(1)> b",
            "<b>x</b>",
        ] {
            let h = html(src);
            assert!(
                !h.contains("<script") && !h.contains("<img") && !h.contains("<b>"),
                "{h}"
            );
            assert!(h.contains("&lt;"), "{h}");
        }
        assert_eq!(html("World<T>"), "<p>World&lt;T&gt;</p>");
    }

    #[test_id(t3p2zvk)]
    #[test]
    fn only_safe_links_stay_links() {
        for src in [
            "[x](javascript:alert(1))",
            "[x](JaVaScRiPt:alert(1))",
            "[x](data:text/html,a)",
            "[x](//e.com)",
        ] {
            let h = html(src);
            assert!(!h.contains("<a") && !h.contains("href"), "{h}");
            assert!(h.contains('x'), "{h}");
        }
        for src in ["[x](mailto:a@b.c)", "[x](#top)", "[x](HTTP://e.com)"] {
            assert!(html(src).contains("<a href="), "{src}");
        }
    }

    #[test_id(tnsktvm)]
    #[test]
    fn an_image_is_its_alt_text() {
        let h = html("![the alt](https://e.com/a.png)");
        assert_eq!(h, "<p>the alt</p>");
    }
}
