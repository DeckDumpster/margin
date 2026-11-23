//! Drives Playwright's WebKit (a touch phone) against an in-process server, because WebKit and
//! Chromium differ in which events a touch tap produces. Run by `gate.sh` with `-- --ignored`;
//! refuses by name when `MARGIN_WEBKIT` is unset. Waits are on signals from the page, never on
//! elapsed time.

use margin_test_id::test_id;

mod support;

use support::webkit::WebKit;

#[test_id(tzgknyn)]
#[tokio::test]
#[ignore = "needs MARGIN_WEBKIT; gate.sh runs it with --ignored"]
async fn a_webkit_touch_tap_opens_boxes_from_a_line_number_and_from_selected_words() {
    let (url, _dir) = support::serve().await;
    let mut wk = WebKit::open(&url).await;

    // The driver's taps reach the page: a line number opens a box and Cancel removes it.
    wk.eval::<bool>("(() => { document.querySelector('[data-line=\"12\"]').scrollIntoView({block: 'center'}); return true; })()")
        .await;
    wk.tap_on("[data-line=\"12\"] .no").await;
    wk.wait_for(
        "a tap on a line number opened the box",
        "document.querySelector('.newbox')",
    )
    .await;
    wk.tap_on(".newbox [data-act=\"cancel\"]").await;
    wk.wait_for(
        "a tap on Cancel removed the box",
        "!document.querySelector('.newbox')",
    )
    .await;

    // Selected words get the Comment button; tapping it opens the box with the columns.
    wk.eval::<bool>(&support::select_js(10, 3, 11, 10)).await;
    wk.wait_for(
        "the Comment button showed",
        "(() => { const b = document.getElementById('hl-comment'); return b && !b.hidden; })()",
    )
    .await;
    wk.tap_on("#hl-comment").await;
    wk.wait_for(
        "the tap on Comment opened the box with columns",
        "document.querySelector('.newbox[data-start-col]')",
    )
    .await;
    let anchor: (String, String, String, String) = wk
        .eval(
            "(() => { const f = document.querySelector('.newbox'); return [f.dataset.first, f.dataset.last, f.dataset.startCol, f.dataset.endCol]; })()",
        )
        .await;
    let want = |a: &str, b: &str, c: &str, d: &str| {
        (a.to_string(), b.to_string(), c.to_string(), d.to_string())
    };
    assert_eq!(anchor, want("10", "11", "3", "10"));
}
