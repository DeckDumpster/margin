//! The open page follows the feed: an agent's comment, posted over HTTP, appears in the page
//! with no navigation, and held comments appear when the reader taps Publish. Run by `gate.sh`
//! with `-- --ignored`; refuses by name when `MARGIN_CHROMIUM` is unset. Waits are on signals
//! from the page, never on elapsed time. Screenshots are evidence only.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use margin::agents::{OsRng, hash_token};
use margin::api::{AppState, router};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin::store::{Kind, NewCollection, NewDocument};
use margin_core::SystemClock;
use margin_test_id::test_id;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "live-token-0123456789";
const SIGNAL_DEADLINE: Duration = Duration::from_secs(20);

/// Sixty lines: two documents of it scroll at 390x844.
fn original() -> String {
    (1..=60).map(|n| format!("fn line_{n}() {{}}\n")).collect()
}

/// `original` with `edit` applied to its lines.
fn revised(edit: impl FnOnce(&mut Vec<String>)) -> String {
    let mut lines: Vec<String> = original().lines().map(String::from).collect();
    edit(&mut lines);
    lines.iter().map(|l| format!("{l}\n")).collect()
}

struct Rig {
    browser: Browser,
    page: Page,
    pump: tokio::task::JoinHandle<()>,
    addr: std::net::SocketAddr,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    let chromium = std::env::var("MARGIN_CHROMIUM").expect(
        "MARGIN_CHROMIUM must name a chromium binary; this suite refuses to run without it",
    );
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents.toml");
    std::fs::write(&agents, format!("ada = \"{}\"\n", hash_token(TOKEN))).unwrap();
    let config = Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\nbase_url = \"http://127.0.0.1\"\ndatabase = \"{}\"\nagents_file = \"{}\"\notlp_endpoint = \"http://127.0.0.1:1\"\ninstance = \"test\"\n",
        dir.path().join("m.db").display(),
        agents.display()
    ))
    .unwrap();
    let state = AppState {
        db: Db::open(&config.database).unwrap(),
        config: Arc::new(config),
        clock: Arc::new(SystemClock),
        rng: Arc::new(OsRng),
        metrics: Metrics::detached(),
        highlighter: Arc::new(Highlighter::new()),
    };
    let text = original();
    state
        .db
        .call(move |conn| {
            let docs = vec![
                (
                    "d1".to_string(),
                    NewDocument {
                        path: "a.rs".into(),
                        kind: Kind::Code,
                        text: text.clone(),
                    },
                ),
                (
                    "d2".to_string(),
                    NewDocument {
                        path: "b.rs".into(),
                        kind: Kind::Code,
                        text,
                    },
                ),
            ];
            margin::store::create(
                conn,
                &NewCollection {
                    id: "live",
                    title: "Live",
                    cover_note: "",
                    created_by: "ui",
                    now: 0,
                    documents: &docs,
                },
            )
        })
        .await
        .unwrap()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let config = BrowserConfig::builder()
        .chrome_executable(chromium)
        .user_data_dir(dir.path().join("chromium-profile"))
        .no_sandbox()
        .args([
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--hide-scrollbars",
        ])
        .build()
        .unwrap();
    let (browser, mut handler) = Browser::launch(config).await.unwrap();
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser.new_page("about:blank").await.unwrap();
    viewport(&page, 390, 844).await;
    page.goto(format!("http://{addr}/c/live"))
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();
    Rig {
        browser,
        page,
        pump,
        addr,
        _dir: dir,
    }
}

impl Rig {
    async fn close(mut self) {
        self.browser.close().await.unwrap();
        let _ = self.pump.await;
    }

    /// One agent request over a plain socket: (status, JSON body).
    async fn agent(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let body = body.to_string();
        let mut sock = tokio::net::TcpStream::connect(self.addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nhost: margin\r\nauthorization: Bearer {TOKEN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).await.unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let status = raw.split(' ').nth(1).unwrap().parse().unwrap();
        let json = raw
            .split("\r\n\r\n")
            .nth(1)
            .and_then(|b| b.find('{').map(|i| &b[i..]))
            .and_then(|b| b.rfind('}').map(|j| &b[..=j]))
            .and_then(|b| serde_json::from_str(b).ok())
            .unwrap_or(Value::Null);
        (status, json)
    }

    /// Post the next version of document `doc` and wait for the page to show `version`.
    async fn post_version(&self, doc: &str, text: &str, section: u32, version: u32) {
        let (status, made) = self
            .agent(
                "POST",
                &format!("/api/documents/{doc}/versions"),
                json!({"text": text}),
            )
            .await;
        assert_eq!(status, 201, "{made}");
        wait_for(
            &self.page,
            "the new version label",
            &format!(
                "document.querySelector('#doc-{section} .version').textContent === 'version {version}'"
            ),
        )
        .await;
    }

    async fn agent_thread(&self, line: u32, body: &str, held: bool) -> String {
        let (status, made) = self
            .agent(
                "POST",
                "/api/collections/live/threads",
                json!({"path": "a.rs", "first_line": line, "last_line": line, "body": body, "held": held}),
            )
            .await;
        assert_eq!(status, 201, "{made}");
        made["id"].as_str().unwrap().to_string()
    }
}

async fn viewport(page: &Page, width: u32, height: u32) {
    page.execute(SetDeviceMetricsOverrideParams::new(
        width as i64,
        height as i64,
        1.0,
        width < 600,
    ))
    .await
    .unwrap();
}

async fn eval<T: serde::de::DeserializeOwned>(page: &Page, js: &str) -> T {
    page.evaluate(js)
        .await
        .unwrap_or_else(|e| panic!("evaluating {js}: {e}"))
        .into_value()
        .unwrap_or_else(|e| panic!("reading the value of {js}: {e}"))
}

/// Poll `js` until it is truthy: the page's own signal, with a deadline only to fail loudly.
async fn wait_for(page: &Page, what: &str, js: &str) {
    let started = Instant::now();
    loop {
        if eval::<bool>(page, &format!("Boolean({js})")).await {
            return;
        }
        assert!(
            started.elapsed() < SIGNAL_DEADLINE,
            "the page never signalled: {what} ({js})"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn has_text(text: &str) -> String {
    format!("document.body.textContent.includes({text:?})")
}

async fn screenshot(page: &Page, name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    page.save_screenshot(ScreenshotParams::builder().build(), dir.join(name))
        .await
        .unwrap();
}

#[test_id(t5uh8na)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn an_agent_comment_appears_without_a_reload_and_held_ones_wait_for_publish() {
    let rig = rig().await;
    let page = &rig.page;
    eval::<bool>(page, "(window.__still_here = true)").await;

    let tid = rig.agent_thread(3, "agent opens a thread", false).await;
    wait_for(
        page,
        "the agent's thread",
        &has_text("agent opens a thread"),
    )
    .await;

    let (status, _) = rig
        .agent(
            "POST",
            &format!("/api/threads/{tid}/comments"),
            json!({"body": "agent replies in it"}),
        )
        .await;
    assert_eq!(status, 201);
    wait_for(
        page,
        "the reply inside the same thread",
        &format!(
            "document.getElementById('thread-{tid}').textContent.includes('agent replies in it')"
        ),
    )
    .await;

    // A held thread, then a visible one after it: once the visible one is on the page the feed
    // has gone past the held one, and the page does not show it.
    let held = rig.agent_thread(8, "held until publish", true).await;
    rig.agent_thread(12, "visible after the held one", false)
        .await;
    wait_for(
        page,
        "the later visible thread",
        &has_text("visible after the held one"),
    )
    .await;
    assert!(!eval::<bool>(page, &has_text("held until publish")).await);
    assert!(
        eval::<bool>(page, "window.__still_here === true").await,
        "the page navigated"
    );

    let publish_visible = "(() => { const b = document.getElementById('publish'); return !!b && !b.hidden && b.getBoundingClientRect().width > 0; })()";
    assert!(eval::<bool>(page, publish_visible).await);
    eval::<bool>(
        page,
        "document.getElementById('publish').scrollIntoView(); true",
    )
    .await;
    screenshot(page, "live-publish-390.png").await;
    viewport(page, 1280, 844).await;
    eval::<bool>(
        page,
        "document.getElementById('publish').scrollIntoView(); true",
    )
    .await;
    screenshot(page, "live-publish-1280.png").await;
    viewport(page, 390, 844).await;

    eval::<bool>(page, "document.getElementById('publish').click(); true").await;
    wait_for(
        page,
        "the held thread",
        &format!("document.getElementById('thread-{held}')"),
    )
    .await;
    assert!(eval::<bool>(page, &has_text("held until publish")).await);
    assert!(
        eval::<bool>(page, "window.__still_here === true").await,
        "the page navigated"
    );

    for (width, name) in [
        (390, "live-revealed-390.png"),
        (1280, "live-revealed-1280.png"),
    ] {
        viewport(page, width, 844).await;
        eval::<bool>(
            page,
            &format!(
                "(() => {{ const t = document.getElementById('thread-{held}'); t.hidden = false;
                  t.scrollIntoView({{block: 'center'}}); return true; }})()"
            ),
        )
        .await;
        screenshot(page, name).await;
    }
    rig.close().await;
}

/// Open the new-comment box under `line` of document `section` by tapping its number, and type
/// `text` into it.
async fn draft(page: &Page, section: u32, line: u32, text: &str) {
    eval::<bool>(
        page,
        &format!(
            "(() => {{
              const no = document.querySelector('#doc-{section} [data-line=\"{line}\"] .no');
              const ev = (t) => new PointerEvent(t, {{bubbles: true, pointerId: 1, pointerType: 'touch', clientX: 3, clientY: 3}});
              no.dispatchEvent(ev('pointerdown'));
              no.dispatchEvent(ev('pointerup'));
              const area = document.querySelector('form.newbox textarea');
              area.value = {text:?};
              area.focus();
              return true;
            }})()"
        ),
    )
    .await;
}

const BOX_TOP: &str = "document.querySelector('form.newbox').getBoundingClientRect().top";

async fn number(page: &Page, js: &str) -> f64 {
    eval::<f64>(page, js).await
}

fn within_2px(was: f64, now: f64) {
    assert!((was - now).abs() <= 2.0, "moved from {was} to {now}");
}

#[test_id(t7195j8)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_version_of_another_document_keeps_the_draft_the_open_thread_and_the_place() {
    let rig = rig().await;
    let page = &rig.page;
    let tid = rig.agent_thread(8, "an open thread", false).await;
    wait_for(page, "the thread", &has_text("an open thread")).await;
    draft(page, 1, 12, "half-written draft").await;
    eval::<bool>(
        page,
        &format!(
            "(document.querySelector('.mark[data-thread=\"{tid}\"]').click(), window.__still_here = true)"
        ),
    )
    .await;
    wait_for(
        page,
        "the thread open",
        &format!("!document.getElementById('thread-{tid}').hidden"),
    )
    .await;
    eval::<bool>(page, "(window.scrollTo(0, 200), true)").await;
    let top = number(page, BOX_TOP).await;

    rig.post_version("d2", &revised(|l| l.insert(0, "// new".into())), 2, 2)
        .await;

    assert_eq!(
        eval::<u32>(page, "document.querySelectorAll('form.newbox').length").await,
        1
    );
    assert!(
        eval::<bool>(
            page,
            "document.querySelector('form.newbox textarea').value === 'half-written draft' && document.activeElement === document.querySelector('form.newbox textarea')"
        )
        .await,
        "the draft or its focus was lost"
    );
    assert!(
        eval::<bool>(
            page,
            &format!("!document.getElementById('thread-{tid}').hidden")
        )
        .await
    );
    within_2px(top, number(page, BOX_TOP).await);
    assert!(eval::<bool>(page, "window.__still_here === true").await);

    for (width, name) in [(390, "live-draft-390.png"), (1280, "live-draft-1280.png")] {
        viewport(page, width, 844).await;
        eval::<bool>(
            page,
            "(document.querySelector('form.newbox').scrollIntoView({block: 'center'}), true)",
        )
        .await;
        screenshot(page, name).await;
    }
    rig.close().await;
}

#[test_id(tk2zip8)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_version_that_moves_the_line_moves_the_draft_with_it_and_posting_lands_there() {
    let rig = rig().await;
    let page = &rig.page;
    draft(page, 1, 12, "moves with its line").await;
    eval::<bool>(
        page,
        "(window.scrollTo(0, 200), window.__still_here = true)",
    )
    .await;
    let top = number(page, BOX_TOP).await;

    rig.post_version(
        "d1",
        &revised(|l| {
            for n in 0..3 {
                l.insert(0, format!("// inserted {n}"));
            }
        }),
        1,
        2,
    )
    .await;

    assert!(
        eval::<bool>(
            page,
            "(() => { const f = document.querySelector('form.newbox');
              return f.querySelector('textarea').value === 'moves with its line'
                && f.querySelector('.who').textContent === 'Line 15'
                && f.previousElementSibling.getAttribute('data-line') === '15'
                && !f.querySelector('.carry'); })()"
        )
        .await,
        "the box did not move to line 15 with its text"
    );
    within_2px(top, number(page, BOX_TOP).await);
    assert!(eval::<bool>(page, "window.__still_here === true").await);
    assert!(
        eval::<bool>(
            page,
            "document.documentElement.scrollWidth <= window.innerWidth"
        )
        .await,
        "the page scrolls sideways"
    );

    eval::<bool>(
        page,
        "(document.querySelector('form.newbox').requestSubmit(), true)",
    )
    .await;
    wait_for(
        page,
        "the thread under line 15 of the new version",
        "(() => { const t = document.querySelector('#doc-1 .thread');
          return t && t.textContent.includes('moves with its line')
            && t.previousElementSibling.getAttribute('data-line') === '15'; })()",
    )
    .await;
    rig.close().await;
}

#[test_id(td4iusv)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_version_that_deletes_the_line_leaves_the_draft_at_the_nearest_surviving_line() {
    let rig = rig().await;
    let page = &rig.page;
    draft(page, 1, 12, "outlives its line").await;
    eval::<bool>(page, "(window.__still_here = true)").await;

    rig.post_version("d1", &revised(|l| drop(l.remove(11))), 1, 2)
        .await;

    assert!(
        eval::<bool>(
            page,
            "(() => { const f = document.querySelector('form.newbox');
              return !!f && f.querySelector('textarea').value === 'outlives its line'
                && f.previousElementSibling.getAttribute('data-line') === '11'
                && f.textContent.includes('on a line since removed'); })()"
        )
        .await,
        "the draft is not at line 11 labelled as removed"
    );
    assert!(eval::<bool>(page, "window.__still_here === true").await);
    rig.close().await;
}

/// The reader's own thread on `line` of document 1, made through the box.
async fn reader_thread(page: &Page, line: u32, text: &str) {
    draft(page, 1, line, text).await;
    eval::<bool>(
        page,
        "(document.querySelector('form.newbox').requestSubmit(), true)",
    )
    .await;
    wait_for(
        page,
        "the reader's thread",
        &format!("document.querySelector('#doc-1 .thread') && !document.querySelector('form.newbox') && {}", has_text(text)),
    )
    .await;
}

#[test_id(ti88jbt)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn an_unposted_reply_and_an_open_edit_survive_a_version_and_both_save() {
    let rig = rig().await;
    let page = &rig.page;
    reader_thread(page, 5, "first thread").await;
    reader_thread(page, 9, "second thread").await;
    eval::<bool>(
        page,
        "(() => {
          const [a, b] = document.querySelectorAll('#doc-1 .thread');
          a.querySelector('form.reply textarea').value = 'unposted reply';
          b.querySelector('button[data-act=edit]').click();
          b.querySelector('form.edit textarea').value = 'edited wording';
          window.__still_here = true;
          return true;
        })()",
    )
    .await;

    rig.post_version("d1", &revised(|l| l.insert(0, "// inserted".into())), 1, 2)
        .await;

    assert!(
        eval::<bool>(
            page,
            "(() => {
              const [a, b] = document.querySelectorAll('#doc-1 .thread');
              return !a.hidden && !b.hidden
                && a.querySelector('form.reply textarea').value === 'unposted reply'
                && b.querySelector('form.edit textarea').value === 'edited wording'
                && b.querySelector('form.edit')._body.textContent === 'second thread';
            })()"
        )
        .await,
        "the reply or the edit was lost"
    );
    eval::<bool>(
        page,
        "(() => {
          const [a, b] = document.querySelectorAll('#doc-1 .thread');
          b.querySelector('form.edit').requestSubmit();
          a.querySelector('form.reply').requestSubmit();
          return true;
        })()",
    )
    .await;
    wait_for(
        page,
        "both saved",
        "(() => {
          const [a, b] = document.querySelectorAll('#doc-1 .thread');
          return a.textContent.includes('unposted reply') && !a.querySelector('form.reply textarea').value
            && b.textContent.includes('edited wording') && !b.querySelector('form.edit');
        })()",
    )
    .await;
    assert!(eval::<bool>(page, "window.__still_here === true").await);
    rig.close().await;
}

#[test_id(t7v5lin)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_version_above_leaves_the_row_on_screen_where_it_was_with_no_form_open() {
    let rig = rig().await;
    let page = &rig.page;
    eval::<bool>(
        page,
        "(document.querySelector('#doc-2 [data-line=\"20\"]').scrollIntoView(), window.scrollBy(0, -30), window.__still_here = true)",
    )
    .await;
    let row = "document.querySelector('#doc-2 [data-line=\"20\"]').getBoundingClientRect().top";
    let top = number(page, row).await;

    rig.post_version(
        "d1",
        &revised(|l| {
            for n in 0..5 {
                l.insert(0, format!("// inserted {n}"));
            }
        }),
        1,
        2,
    )
    .await;

    within_2px(top, number(page, row).await);
    assert!(eval::<bool>(page, "window.__still_here === true").await);
    rig.close().await;
}

#[test_id(thp0uc1)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_new_cover_note_shows_and_keeps_the_draft() {
    let rig = rig().await;
    let page = &rig.page;
    draft(page, 1, 12, "draft beside a cover note").await;
    eval::<bool>(page, "(window.__still_here = true)").await;

    let (status, got) = rig
        .agent(
            "PATCH",
            "/api/collections/live",
            json!({"cover_note": "read this first"}),
        )
        .await;
    assert_eq!(status, 200, "{got}");
    wait_for(page, "the cover note", "document.querySelector('.cover') && document.querySelector('.cover').textContent === 'read this first'").await;

    assert!(
        eval::<bool>(
            page,
            "document.querySelector('form.newbox textarea').value === 'draft beside a cover note' && window.__still_here === true"
        )
        .await
    );
    rig.close().await;
}

#[test_id(tng0ttl)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_markdown_cover_note_shows_formatted_without_a_reload() {
    let rig = rig().await;
    let page = &rig.page;
    eval::<bool>(page, "(window.__still_here = true)").await;
    let (status, got) = rig
        .agent(
            "PATCH",
            "/api/collections/live",
            json!({"cover_note": "## Heading\n\n- one\n- two"}),
        )
        .await;
    assert_eq!(status, 200, "{got}");
    wait_for(page, "the formatted cover", "document.querySelector('.cover h2') && document.querySelectorAll('.cover li').length === 2").await;
    assert!(eval::<bool>(page, "window.__still_here === true").await);
    rig.close().await;
}

#[test_id(trpolbp)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_page_fetch_that_fails_once_keeps_the_draft_and_the_version_then_appears() {
    let rig = rig().await;
    let page = &rig.page;
    draft(page, 1, 12, "survives a failed fetch").await;
    eval::<bool>(
        page,
        "(() => {
          const real = window.fetch;
          window.__refused = 0;
          window.fetch = function (u) {
            if (u === '/c/live' && window.__refused === 0) {
              window.__refused = 1;
              return Promise.reject(new TypeError('offline'));
            }
            return real.apply(this, arguments);
          };
          window.__still_here = true;
          return true;
        })()",
    )
    .await;

    rig.post_version("d1", &revised(|l| l.insert(0, "// inserted".into())), 1, 2)
        .await;

    assert!(
        eval::<bool>(
            page,
            "window.__refused === 1 && document.querySelector('form.newbox textarea').value === 'survives a failed fetch' && window.__still_here === true"
        )
        .await
    );
    rig.close().await;
}
