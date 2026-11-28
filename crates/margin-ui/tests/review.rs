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
        "cover_note": "## Summary\n\n- a **bold** point\n- see `src/one.rs:2000`\n- and `src/two.rs:1`\n",
        "documents": [
            {"path": "src/one.rs", "kind": "code", "text": support::FIXTURE},
            {"path": "src/two.rs", "kind": "code", "text": "fn two() {}\n"},
            {"path": "src/three.rs", "kind": "code", "text": "fn three() {}\n"},
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
        3
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

    // The bar's buttons cross pages at a page's edge: one.rs is all of page 1.
    eval::<bool>(
        &page,
        "(document.querySelector('#doc-1').scrollIntoView(), true)",
    )
    .await;
    wait_for(
        &page,
        "the bar naming page 1's file",
        "!document.querySelector('nav.bar').hidden && document.querySelector('#bar-file').textContent === 'one.rs'",
    )
    .await;
    assert!(
        !eval::<bool>(&page, "document.querySelector('#bar-next').disabled").await,
        "bar-next is disabled on the page's last file, with a page after it"
    );
    eval::<bool>(&page, "(document.querySelector('#bar-next').click(), true)").await;
    wait_for(
        &page,
        "the second page by the bar's next button",
        "location.search === '?page=2' && document.readyState === 'complete' && document.querySelector('#doc-2')
           && !document.querySelector('nav.bar').hidden && document.querySelector('#bar-file').textContent === 'two.rs'",
    )
    .await;
    assert!(
        !eval::<bool>(&page, "document.querySelector('#bar-prev').disabled").await,
        "bar-prev is disabled on the page's first file, with a page before it"
    );
    eval::<bool>(&page, "(document.querySelector('#bar-prev').click(), true)").await;
    wait_for(
        &page,
        "the first page by the bar's previous button",
        "location.search === '?page=1' && document.readyState === 'complete' && document.querySelector('#doc-1')",
    )
    .await;

    // A link, then an address with no page number, naming a line on another page.
    let landed = "location.search === '?page=2' && location.hash === '#doc-2-1' && document.readyState === 'complete' && document.querySelector('#doc-2-1')";
    eval::<bool>(
        &page,
        "(document.querySelector('.cover a[href=\"#doc-2-1\"]').click(), true)",
    )
    .await;
    wait_for(&page, "the line a link names on another page", landed).await;
    // The marker tells the page the address loads from the one the link left, which looks alike.
    eval::<bool>(&page, "(window.__before = true)").await;
    page.goto(format!("{home}c/{number}#doc-2-1"))
        .await
        .unwrap();
    wait_for(
        &page,
        "the line an address names on another page",
        &format!("!window.__before && {landed}"),
    )
    .await;

    // The review's id, then its documents' ids in order, for the changes an agent makes below.
    let ids: Vec<String> = eval(
        &page,
        &format!(
            "(() => {{ const id = document.body.getAttribute('data-collection');
               return fetch('/api/collections/' + id, {{headers: {{authorization: 'Bearer {TOKEN}'}}}})
                 .then(r => r.json()).then(j => [id, ...j.documents.map(d => d.id)]); }})()"
        ),
    )
    .await;
    let post = |path: String, body: &str| {
        format!(
            "fetch('{path}', {{method: 'POST', headers: {{authorization: 'Bearer {TOKEN}', 'content-type': 'application/json'}}, body: JSON.stringify({body})}})
               .then(r => {{ if (r.status !== 201) throw new Error('{path} answered ' + r.status); return r.json(); }})"
        )
    };
    // `doc` is the document's `N` in `#doc-N`; `text` is a script expression.
    let new_version = |doc: usize, text: &str| {
        let path = format!("/api/documents/{}/versions", ids[doc]);
        format!(
            "{}.then(() => true)",
            post(path, &format!("{{text: {text}}}"))
        )
    };

    // A new version of a file on page 2 refreshes that page in place.
    eval::<bool>(&page, "(window.__kept = true)").await;
    eval::<bool>(&page, &new_version(2, r"'fn two() { 2 }\n'")).await;
    wait_for(
        &page,
        "page 2 refreshed in place",
        "window.__kept === true && location.search === '?page=2' && document.querySelector('#doc-2')?.textContent.includes('2 }')",
    )
    .await;

    // The files list counts comments on a file this page does not show.
    let path = format!("/api/collections/{}/threads", ids[0]);
    let made = post(
        path,
        "{path: 'src/one.rs', first_line: 5, last_line: 5, body: 'from afar'}",
    );
    let comment: String = eval(&page, &format!("{made}.then(t => t.comments[0].id)")).await;
    let one_count = "document.querySelector('.files a[data-doc=\"1\"] .fsum').textContent";
    wait_for(
        &page,
        "a comment added on another page counted",
        &format!("{one_count}.endsWith(' 2 comments')"),
    )
    .await;
    let deleted: u16 = eval(
        &page,
        &format!(
            "fetch('/api/comments/{comment}', {{method: 'DELETE', headers: {{authorization: 'Bearer {TOKEN}'}}}}).then(r => r.status)"
        ),
    )
    .await;
    assert_eq!(deleted, 204);
    wait_for(
        &page,
        "a comment deleted on another page uncounted",
        &format!("{one_count}.endsWith(' 1 comment')"),
    )
    .await;

    // Opens a box on line 1 of document `doc` and types `text` into it, without posting.
    let draft = |doc: usize, text: &str| {
        format!(
            "(() => {{ const no = document.querySelector('#doc-{doc} [data-line=\"1\"] .no');
               const o = {{bubbles: true, pointerId: 1, pointerType: 'mouse', button: 0}};
               no.dispatchEvent(new PointerEvent('pointerdown', o));
               no.dispatchEvent(new PointerEvent('pointerup', o));
               document.querySelector('#doc-{doc} .newbox textarea').value = '{text}'; return true; }})()"
        )
    };
    let draft_of =
        |doc: usize| format!("document.querySelector('#doc-{doc} .newbox textarea')?.value ?? ''");

    // A new version pushes three.rs to a third page; the unposted text keeps its file here.
    eval::<bool>(&page, &draft(3, "kept on a move")).await;
    eval::<bool>(
        &page,
        &new_version(
            2,
            r"Array.from({length: 1600}, (_, i) => '// ' + i).join('\n') + '\n'",
        ),
    )
    .await;
    wait_for(
        &page,
        "the longer file on page 2",
        "document.querySelectorAll('#doc-2 .ln').length === 1600",
    )
    .await;
    assert_eq!(
        eval::<String>(&page, &draft_of(3)).await,
        "kept on a move",
        "unposted text is lost when a new version moves its file to another page"
    );
    assert_eq!(eval::<String>(&page, "location.search").await, "?page=2");
    eval::<bool>(
        &page,
        "(document.querySelector('#doc-3 .newbox [data-act=cancel]').click(), true)",
    )
    .await;

    // New versions leave one page. A refused page changes nothing on screen, so the sign is the
    // script's own request: a second 404 means the page stayed through the first and asked again.
    eval::<bool>(&page, &draft(2, "kept on a removal")).await;
    eval::<bool>(
        &page,
        "(() => { const real = window.fetch; window.__refused = 0;
           window.fetch = (u, o) => real(u, o).then(r => {
             if (r.status === 404 && String(u).endsWith('?page=2')) window.__refused++;
             return r; });
           return true; })()",
    )
    .await;
    eval::<bool>(&page, &new_version(1, r"'fn one() {}\n'")).await;
    eval::<bool>(&page, &new_version(2, r"'fn two() {}\n'")).await;
    wait_for(
        &page,
        "page 2 refused twice, the page still here",
        "window.__refused >= 2",
    )
    .await;
    assert_eq!(
        eval::<String>(&page, &draft_of(2)).await,
        "kept on a removal",
        "unposted text is lost with the page that held it"
    );
    assert_eq!(eval::<String>(&page, "location.search").await, "?page=2");
    eval::<bool>(
        &page,
        "(document.querySelector('#doc-2 .newbox textarea').value = '', true)",
    )
    .await;
    wait_for(
        &page,
        "the first page, holding every file",
        "location.search === '' && document.querySelector('#doc-1') && document.querySelector('#doc-2') && document.querySelector('#doc-3')",
    )
    .await;

    browser.close().await.ok();
    pump.abort();
}
