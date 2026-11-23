//! Drives Playwright's WebKit (a touch phone) against an in-process server, because WebKit and
//! Chromium differ in which events a touch tap produces. Run by `gate.sh` with `-- --ignored`;
//! refuses by name when `MARGIN_WEBKIT` is unset. Waits are on signals from the page, never on
//! elapsed time.

use margin_test_id::test_id;

mod support;

use support::webkit::WebKit;

const SELECT_WORDS: &str = "(() => {
  const at = (n, col) => {
    const code = document.querySelector('[data-line=\"' + n + '\"] .code');
    const walker = document.createTreeWalker(code, NodeFilter.SHOW_TEXT);
    let left = col;
    for (let t = walker.nextNode(); t; t = walker.nextNode()) {
      if (left <= t.data.length) return [t, left];
      left -= t.data.length;
    }
    throw new Error('column past the line');
  };
  document.querySelector('[data-line=\"10\"]').scrollIntoView({block: 'center'});
  const [a, ao] = at(10, 3);
  const [b, bo] = at(11, 10);
  const r = document.createRange();
  r.setStart(a, ao);
  r.setEnd(b, bo);
  const s = getSelection();
  s.removeAllRanges();
  s.addRange(r);
  return true;
})()";

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
    wk.eval::<bool>(SELECT_WORDS).await;
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
