//! The one browser test: a review is opened through the API, found from the home page, read, and
//! commented on. Run by `gate.sh` with `-- --ignored`; refuses by name when `MARGIN_CHROMIUM` is
//! unset. Waits are on the page's own signals; the deadline only makes a failure loud.

use std::time::{Duration, Instant};

use chromiumoxide::page::Page;
use margin::agents::hash_token;
use margin_test_id::test_id;

mod support;

const TOKEN: &str = "review-token-0123456789";
const SIGNAL_DEADLINE: Duration = Duration::from_secs(20);

async fn eval<T: serde::de::DeserializeOwned>(page: &Page, js: &str) -> T {
    page.evaluate(js)
        .await
        .unwrap_or_else(|e| panic!("evaluating {js}: {e}"))
        .into_value()
        .unwrap_or_else(|e| panic!("reading the value of {js}: {e}"))
}

/// Poll `js` until it is truthy. A poll that lands while the page is navigating has no context
/// to run in and counts as not yet.
async fn wait_for(page: &Page, what: &str, js: &str) {
    let started = Instant::now();
    let js = format!("Boolean({js})");
    loop {
        if let Ok(v) = page.evaluate(js.as_str()).await
            && v.into_value::<bool>().unwrap_or(false)
        {
            return;
        }
        assert!(
            started.elapsed() < SIGNAL_DEADLINE,
            "the page never signalled: {what} ({js})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test_id(tq7c2rv)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_review_is_opened_found_read_and_commented_on() {
    let agents = format!("ada = \"{}\"\n", hash_token(TOKEN));
    let (fixture, dir) =
        support::serve_docs("Fixture", &[("a.rs", "fn a() {}\n".into())], &agents).await;
    let home = fixture.trim_end_matches("c/fixture").to_string();
    let (mut browser, pump) = support::launch(dir.path()).await;
    let page = browser.new_page(&home).await.unwrap();

    let body = serde_json::json!({
        "title": "One review",
        "cover_note": "## Summary\n\n- a **bold** point\n- see `src/one.rs:2000`\n",
        "documents": [
            {"path": "src/one.rs", "kind": "code", "text": support::FIXTURE},
            {"path": "src/two.rs", "kind": "code", "text": "fn two() {}\n"},
        ],
    });
    let number: i64 = eval(
        &page,
        &format!(
            "fetch('/api/collections', {{method: 'POST', headers: {{authorization: 'Bearer {TOKEN}', 'content-type': 'application/json'}}, body: JSON.stringify({body})}}).then(r => r.json()).then(j => j.number)"
        ),
    )
    .await;

    page.goto(&home).await.unwrap();
    let link = format!("a.entry[href=\"/c/{number}\"]");
    wait_for(
        &page,
        "the review on the home page",
        &format!("document.querySelector({link:?})"),
    )
    .await;
    eval::<bool>(
        &page,
        &format!("(document.querySelector({link:?}).click(), true)"),
    )
    .await;
    wait_for(
        &page,
        "the review page",
        "document.readyState === 'complete' && document.querySelector('#doc-1')",
    )
    .await;

    assert!(eval::<bool>(&page, "document.querySelector('.cover h2')?.textContent === 'Summary' && !!document.querySelector('.cover li strong')").await, "the cover note is not rendered markdown");
    assert_eq!(
        eval::<u32>(&page, "document.querySelectorAll('.files a').length").await,
        2
    );

    eval::<bool>(
        &page,
        "(document.querySelector('.cover a[href=\"#doc-1-2000\"]').click(), true)",
    )
    .await;
    wait_for(
        &page,
        "the row of the cover's span below the bar",
        "(() => { const bar = document.querySelector('nav.bar'); const row = document.querySelector('#doc-1-2000');
           const top = row.getBoundingClientRect().top;
           return !bar.hidden && top >= bar.getBoundingClientRect().bottom - 1 && top < window.innerHeight; })()",
    )
    .await;

    // The 3,000-line file fills the first page; the file list reaches the second file's page.
    eval::<bool>(
        &page,
        "([...document.querySelectorAll('.files a')].find(a => a.textContent.includes('src/two.rs')).click(), true)",
    )
    .await;
    wait_for(
        &page,
        "the second page, its file below the bar",
        "(() => { const bar = document.querySelector('nav.bar'); const doc = document.querySelector('#doc-2');
           if (location.search !== '?page=2' || !bar || !doc || bar.hidden) return false;
           const top = doc.getBoundingClientRect().top;
           return top >= bar.getBoundingClientRect().bottom - 1 && top < window.innerHeight
             && document.querySelector('#bar-file').textContent === 'two.rs'; })()",
    )
    .await;
    eval::<bool>(
        &page,
        "(document.querySelector('.pager a[rel=prev]').click(), true)",
    )
    .await;
    // The link lands on the file's section; waiting for the bar to name it means that scroll is
    // done and cannot undo the one to the top below.
    wait_for(
        &page,
        "back on the first page",
        "location.search === '?page=1' && document.readyState === 'complete' && document.querySelector('#doc-1')
           && !document.querySelector('nav.bar').hidden && document.querySelector('#bar-file').textContent === 'one.rs'",
    )
    .await;
    eval::<bool>(&page, "(document.querySelector('#bar-top').click(), true)").await;
    wait_for(&page, "the top of the page", "window.scrollY === 0").await;

    // A mouse drag from a line number selects the lines it crosses at once, with no hold first.
    let painted: u32 = eval(
        &page,
        "(() => { const last = document.querySelector('[data-line=\"3\"]');
           const first = last.parentNode.querySelector('[data-line=\"1\"]');
           const at = (row) => { const r = row.querySelector('.no').getBoundingClientRect();
             return {bubbles: true, pointerId: 7, pointerType: 'mouse', button: 0,
                     clientX: r.left + r.width / 2, clientY: r.top + r.height / 2}; };
           first.querySelector('.no').dispatchEvent(new PointerEvent('pointerdown', at(first)));
           document.dispatchEvent(new PointerEvent('pointermove', at(last)));
           const n = last.parentNode.querySelectorAll('.sel').length;
           document.dispatchEvent(new PointerEvent('pointerup', at(last)));
           return n; })()",
    )
    .await;
    assert_eq!(
        painted, 3,
        "a mouse drag over three line numbers marks three lines"
    );
    wait_for(
        &page,
        "the comment box for the dragged range",
        "document.querySelector('.newbox textarea')",
    )
    .await;
    eval::<bool>(
        &page,
        "(document.querySelector('.newbox [data-act=cancel]').click(), true)",
    )
    .await;
    wait_for(
        &page,
        "the box closed",
        "!document.querySelector('.newbox')",
    )
    .await;

    eval::<bool>(
        &page,
        "(() => { const no = document.querySelector('[data-line=\"3\"] .no');
           const o = {bubbles: true, pointerId: 1, pointerType: 'mouse', button: 0};
           no.dispatchEvent(new PointerEvent('pointerdown', o));
           no.dispatchEvent(new PointerEvent('pointerup', o)); return true; })()",
    )
    .await;
    wait_for(
        &page,
        "the comment box",
        "document.querySelector('.newbox textarea')",
    )
    .await;
    eval::<bool>(
        &page,
        "(() => { const f = document.querySelector('.newbox');
           f.querySelector('textarea').value = 'why this way?';
           f.querySelector('button[type=submit]').click(); return true; })()",
    )
    .await;
    wait_for(&page, "the new thread", "!document.querySelector('.newbox') && document.querySelector('.thread')?.textContent.includes('why this way?')").await;
    let kept: u32 = eval(
        &page,
        &format!("fetch('/c/{number}').then(r => r.text()).then(t => new DOMParser().parseFromString(t, 'text/html').querySelectorAll('.thread').length)"),
    )
    .await;
    assert_eq!(kept, 1);

    browser.close().await.ok();
    pump.abort();
}
