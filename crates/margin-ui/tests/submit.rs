//! A form with a request in flight cannot send another, and a request that gets no answer says so.
//! Requests are held and failed with the CDP `Fetch` domain. Run by `gate.sh` with `-- --ignored`;
//! refuses by name when `MARGIN_CHROMIUM` is unset. Waits are on signals from the page or from
//! the paused-request events, never on elapsed time. Screenshots are evidence only.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams, EventRequestPaused, FailRequestParams,
    FulfillRequestParams, RequestId, RequestPattern,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
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

mod support;

const TOKEN: &str = "submit-token-0123456789";
const SIGNAL_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
struct Held {
    id: RequestId,
    method: String,
    marker: bool,
}

struct Rig {
    browser: Browser,
    page: Page,
    pump: tokio::task::JoinHandle<()>,
    addr: std::net::SocketAddr,
    held: Arc<Mutex<Vec<Held>>>,
    hold: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
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
    let text: String = (1..=40).map(|n| format!("fn line_{n}() {{}}\n")).collect();
    state
        .db
        .call(move |conn| {
            let docs = vec![(
                "d1".to_string(),
                NewDocument {
                    path: "a.rs".into(),
                    kind: Kind::Code,
                    text,
                },
            )];
            margin::store::create(
                conn,
                &NewCollection {
                    id: "submit",
                    title: "Submit",
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
    let (browser, pump) = support::launch(dir.path()).await;
    let page = browser.new_page("about:blank").await.unwrap();
    viewport(&page, 390, 844).await;

    // Requests to the collection's threads and comments are paused; while `hold` is off they
    // pass at once, and while it is on they wait in `held` for the test to release or fail them.
    let held = Arc::new(Mutex::new(Vec::new()));
    let hold = Arc::new(AtomicBool::new(false));
    let mut events = page.event_listener::<EventRequestPaused>().await.unwrap();
    {
        let (held, hold, page) = (held.clone(), hold.clone(), page.clone());
        tokio::spawn(async move {
            while let Some(ev) = events.next().await {
                if hold.load(Ordering::SeqCst) {
                    held.lock().unwrap().push(Held {
                        id: ev.request_id.clone(),
                        method: ev.request.method.clone(),
                        marker: ev.request.headers.inner().get("x-marker").is_some(),
                    });
                } else {
                    let _ = page
                        .execute(ContinueRequestParams::new(ev.request_id.clone()))
                        .await;
                }
            }
        });
    }
    page.execute(
        EnableParams::builder()
            .pattern(
                RequestPattern::builder()
                    .url_pattern("*/c/submit/threads*")
                    .build(),
            )
            .pattern(
                RequestPattern::builder()
                    .url_pattern("*/c/submit/comments*")
                    .build(),
            )
            .build(),
    )
    .await
    .unwrap();
    page.goto(format!("http://{addr}/c/submit"))
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();
    eval::<bool>(
        &page,
        "(window.__rejections = 0, addEventListener('unhandledrejection', () => window.__rejections++), true)",
    )
    .await;
    Rig {
        browser,
        page,
        pump,
        addr,
        held,
        hold,
        _dir: dir,
    }
}

impl Rig {
    async fn close(mut self) {
        self.hold.store(false, Ordering::SeqCst);
        self.browser.close().await.unwrap();
        let _ = self.pump.await;
    }

    fn held(&self) -> Vec<Held> {
        self.held.lock().unwrap().clone()
    }

    /// Wait until `n` requests are held.
    async fn wait_held(&self, n: usize) {
        let started = Instant::now();
        while self.held().len() < n {
            assert!(
                started.elapsed() < SIGNAL_DEADLINE,
                "never {n} paused requests: {:?}",
                self.held()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn fail_held(&self, at: usize) {
        let id = self.held()[at].id.clone();
        self.page
            .execute(FailRequestParams::new(id, ErrorReason::ConnectionFailed))
            .await
            .unwrap();
    }

    /// Let every held request, and every later one, through.
    async fn release_all(&self) {
        self.hold.store(false, Ordering::SeqCst);
        let all: Vec<Held> = self.held.lock().unwrap().drain(..).collect();
        for h in all {
            let _ = self.page.execute(ContinueRequestParams::new(h.id)).await;
        }
    }

    /// Send a POST of `{}` to the threads URL from the page, and wait until it is paused. Every
    /// request the page sent before it is paused before it, so the held list up to the marker is
    /// exactly what the page sent.
    async fn held_before_marker(&self) -> Vec<Held> {
        eval::<bool>(
            &self.page,
            "(fetch('/c/submit/threads', {method: 'POST', headers: {'content-type': 'application/json', 'x-marker': '1'}, body: '{}'}).catch(() => {}), true)",
        )
        .await;
        let started = Instant::now();
        loop {
            let all = self.held();
            if let Some(at) = all.iter().position(|h| h.marker) {
                return all[..at].to_vec();
            }
            assert!(started.elapsed() < SIGNAL_DEADLINE, "marker never paused");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
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

/// Evidence only: nothing asserts on these.
async fn screenshot(page: &Page, name: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    page.evaluate("document.querySelector('.newbox').scrollIntoView({block: 'center'})")
        .await
        .unwrap();
    page.save_screenshot(ScreenshotParams::builder().build(), dir.join(name))
        .await
        .unwrap();
}

/// Tap the number of line `n`, as a pointer does.
async fn tap_line(page: &Page, n: u32) {
    eval::<bool>(
        page,
        &format!(
            "(() => {{ const no = document.querySelector('[data-line=\"{n}\"] .no');
              const o = {{bubbles: true, pointerId: 1, pointerType: 'mouse', button: 0}};
              no.dispatchEvent(new PointerEvent('pointerdown', o));
              no.dispatchEvent(new PointerEvent('pointerup', o)); return true; }})()"
        ),
    )
    .await;
}

/// Type into the form matching `sel` and press its submit button `clicks` times in one turn.
async fn submit(page: &Page, sel: &str, text: &str, clicks: u32) {
    eval::<bool>(
        page,
        &format!(
            "(() => {{ const f = document.querySelector({sel:?});
              f.querySelector('textarea').value = {text:?};
              const b = f.querySelector('button[type=submit]');
              for (let i = 0; i < {clicks}; i++) b.click(); return true; }})()"
        ),
    )
    .await;
}

/// How many threads a fresh fetch of the collection page holds.
async fn threads_on_server(page: &Page) -> u32 {
    eval(
        page,
        "fetch('/c/submit').then(r => r.text()).then(t => new DOMParser().parseFromString(t, 'text/html').querySelectorAll('.thread').length)",
    )
    .await
}

/// Open a box on `line`, post `text` with requests passing, and wait for the thread.
async fn make_thread(rig: &Rig, line: u32, text: &str) {
    tap_line(&rig.page, line).await;
    submit(&rig.page, ".newbox", text, 1).await;
    wait_for(
        &rig.page,
        "the new thread",
        &format!("!document.querySelector('.newbox') && document.querySelector('.thread')?.textContent.includes({text:?})"),
    )
    .await;
}

const NOT_SAVED: &str = "Not saved: no answer from the server";

#[test_id(talpdfj)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_post_with_no_answer_keeps_its_box_and_text_and_can_be_sent_again() {
    let rig = rig().await;
    let page = &rig.page;
    rig.hold.store(true, Ordering::SeqCst);
    tap_line(page, 3).await;
    submit(page, ".newbox", "my first words", 1).await;
    rig.wait_held(1).await;
    rig.fail_held(0).await;
    wait_for(page, "the error", "document.querySelector('.newbox .err')").await;
    assert_eq!(
        eval::<String>(page, "document.querySelector('.newbox .err').textContent").await,
        NOT_SAVED
    );
    assert_eq!(
        eval::<String>(page, "document.querySelector('.newbox textarea').value").await,
        "my first words"
    );
    assert!(
        eval::<bool>(
            page,
            "!document.querySelector('.newbox button[type=submit]').disabled && !document.querySelector('.newbox textarea').readOnly"
        )
        .await
    );
    screenshot(page, "submit-error-390.png").await;
    viewport(page, 1280, 900).await;
    screenshot(page, "submit-error-1280.png").await;
    viewport(page, 390, 844).await;

    submit(page, ".newbox", "my first words", 1).await;
    assert_eq!(
        eval::<u32>(page, "document.querySelectorAll('.newbox .err').length").await,
        0,
        "a new submit removes the earlier error"
    );
    rig.wait_held(2).await;
    rig.release_all().await;
    wait_for(
        page,
        "the box gone",
        "!document.querySelector('.newbox') && document.querySelector('.thread')",
    )
    .await;
    assert_eq!(threads_on_server(page).await, 1);
    assert_eq!(
        eval::<u32>(page, "window.__rejections").await,
        0,
        "an unhandled rejection"
    );
    rig.close().await;
}

#[test_id(tzpcwim)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_reply_and_an_edit_with_no_answer_say_so_and_keep_their_text() {
    let rig = rig().await;
    let page = &rig.page;
    make_thread(&rig, 4, "the opening").await;

    rig.hold.store(true, Ordering::SeqCst);
    submit(page, ".thread form.reply", "a reply that fails", 1).await;
    rig.wait_held(1).await;
    rig.fail_held(0).await;
    wait_for(
        page,
        "the reply error",
        "document.querySelector('.thread form.reply .err')",
    )
    .await;
    assert_eq!(
        eval::<String>(
            page,
            "document.querySelector('.thread form.reply .err').textContent"
        )
        .await,
        NOT_SAVED
    );
    assert_eq!(
        eval::<String>(
            page,
            "document.querySelector('.thread form.reply textarea').value"
        )
        .await,
        "a reply that fails"
    );

    eval::<bool>(
        page,
        "(document.querySelector('.thread button[data-act=edit]').click(), true)",
    )
    .await;
    submit(page, ".thread form.edit", "an edit that fails", 1).await;
    rig.wait_held(2).await;
    rig.fail_held(1).await;
    wait_for(
        page,
        "the edit error",
        "document.querySelector('.thread form.edit .err')",
    )
    .await;
    assert_eq!(
        eval::<String>(
            page,
            "document.querySelector('.thread form.edit .err').textContent"
        )
        .await,
        NOT_SAVED
    );
    assert_eq!(
        eval::<String>(
            page,
            "document.querySelector('.thread form.edit textarea').value"
        )
        .await,
        "an edit that fails"
    );
    assert_eq!(
        eval::<u32>(page, "window.__rejections").await,
        0,
        "an unhandled rejection"
    );
    rig.close().await;
}

#[test_id(tkbg7pj)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn two_taps_on_post_make_one_thread() {
    let rig = rig().await;
    let page = &rig.page;
    rig.hold.store(true, Ordering::SeqCst);
    tap_line(page, 3).await;
    submit(page, ".newbox", "tapped twice", 2).await;
    let before = rig.held_before_marker().await;
    let posts: Vec<_> = before.iter().filter(|h| h.method == "POST").collect();
    assert_eq!(posts.len(), 1, "requests before the marker: {before:?}");
    rig.release_all().await;
    wait_for(page, "the thread", "document.querySelector('.thread')").await;
    assert_eq!(threads_on_server(page).await, 1);
    rig.close().await;
}

#[test_id(t8fkxhc)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_feed_change_during_a_slow_reply_does_not_let_the_reply_go_twice() {
    let rig = rig().await;
    let page = &rig.page;
    make_thread(&rig, 5, "the opening").await;
    let tid = eval::<String>(
        page,
        "document.querySelector('.thread').getAttribute('data-thread')",
    )
    .await;

    rig.hold.store(true, Ordering::SeqCst);
    submit(page, ".thread form.reply", "my slow reply", 1).await;
    rig.wait_held(1).await;
    let (status, _) = rig
        .agent(
            "POST",
            &format!("/api/threads/{tid}/comments"),
            json!({"body": "agent chimes in"}),
        )
        .await;
    assert_eq!(status, 201);
    wait_for(
        page,
        "the agent's comment through the feed",
        "document.querySelector('.thread').textContent.includes('agent chimes in')",
    )
    .await;
    eval::<bool>(
        page,
        "(document.querySelector('.thread form.reply button[type=submit]').click(), true)",
    )
    .await;
    let before = rig.held_before_marker().await;
    assert_eq!(before.len(), 1, "requests before the marker: {before:?}");
    rig.release_all().await;
    wait_for(
        page,
        "the reply in the thread",
        "[...document.querySelectorAll('.thread .cm .body')].some(b => b.textContent.includes('my slow reply'))",
    )
    .await;
    let counts: (u32, u32, String) = eval(
        page,
        "(() => { const t = document.querySelector('.thread');
          const n = s => [...t.querySelectorAll('.cm .body')].filter(b => b.textContent.includes(s)).length;
          return [n('my slow reply'), n('agent chimes in'), t.querySelector('form.reply textarea').value]; })()",
    )
    .await;
    assert_eq!(counts, (1, 1, String::new()));
    rig.close().await;
}

#[test_id(tk7190t)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_post_is_given_five_seconds_and_an_abort_reads_as_no_answer() {
    let rig = rig().await;
    let page = &rig.page;
    rig.hold.store(true, Ordering::SeqCst);
    eval::<bool>(
        page,
        "(() => { window.__timeouts = []; window.__ctl = new AbortController();
          AbortSignal.timeout = ms => { window.__timeouts.push(ms); return window.__ctl.signal; };
          return true; })()",
    )
    .await;
    tap_line(page, 3).await;
    submit(page, ".newbox", "slow words", 1).await;
    rig.wait_held(1).await;
    assert_eq!(
        eval::<Vec<u32>>(page, "window.__timeouts").await,
        vec![5000]
    );
    eval::<bool>(page, "(window.__ctl.abort(), true)").await;
    wait_for(page, "the error", "document.querySelector('.newbox .err')").await;
    assert_eq!(
        eval::<String>(page, "document.querySelector('.newbox .err').textContent").await,
        NOT_SAVED
    );
    rig.close().await;
}

#[test_id(tad82ec)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn tapping_another_line_leaves_a_box_with_a_post_in_flight_alone() {
    let rig = rig().await;
    let page = &rig.page;
    rig.hold.store(true, Ordering::SeqCst);
    tap_line(page, 3).await;
    submit(page, ".newbox", "still in flight", 1).await;
    rig.wait_held(1).await;
    tap_line(page, 9).await;
    let (text, busy): (String, bool) = eval(
        page,
        "(() => { const b = document.querySelector('.newbox[data-first=\"3\"]');
          return [b.querySelector('textarea').value, b.hasAttribute('data-busy')]; })()",
    )
    .await;
    assert_eq!((text.as_str(), busy), ("still in flight", true));
    rig.release_all().await;
    wait_for(
        page,
        "the thread",
        "!document.querySelector('.newbox[data-first=\"3\"]') && document.querySelector('.thread')",
    )
    .await;
    rig.close().await;
}

#[test_id(t3sn8xb)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn an_http_error_still_reads_not_saved_with_its_body_and_frees_the_form() {
    let rig = rig().await;
    let page = &rig.page;
    rig.hold.store(true, Ordering::SeqCst);
    tap_line(page, 3).await;
    submit(page, ".newbox", "refused words", 1).await;
    rig.wait_held(1).await;
    let id = rig.held()[0].id.clone();
    page.execute(
        FulfillRequestParams::builder()
            .request_id(id)
            .response_code(500)
            .body(chromiumoxide::Binary::from("Ym9vbQ==".to_string()))
            .build()
            .unwrap(),
    )
    .await
    .unwrap();
    wait_for(page, "the error", "document.querySelector('.newbox .err')").await;
    assert_eq!(
        eval::<String>(page, "document.querySelector('.newbox .err').textContent").await,
        "Not saved: boom"
    );
    assert!(
        eval::<bool>(
            page,
            "!document.querySelector('.newbox button[type=submit]').disabled && !document.querySelector('.newbox textarea').readOnly && !document.querySelector('.newbox').hasAttribute('data-busy')"
        )
        .await
    );
    rig.close().await;
}

#[test_id(to9m6kv)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn an_answer_for_a_cancelled_box_changes_nothing_and_the_feed_brings_the_thread() {
    let rig = rig().await;
    let page = &rig.page;
    rig.hold.store(true, Ordering::SeqCst);
    tap_line(page, 3).await;
    submit(page, ".newbox", "cancelled in flight", 1).await;
    rig.wait_held(1).await;
    eval::<bool>(
        page,
        "(document.querySelector('.newbox button[data-act=cancel]').click(), true)",
    )
    .await;
    assert_eq!(
        eval::<u32>(page, "document.querySelectorAll('.newbox').length").await,
        0
    );
    rig.release_all().await;
    wait_for(
        page,
        "the thread from the feed",
        "document.querySelector('.thread')",
    )
    .await;
    assert_eq!(
        eval::<u32>(page, "document.querySelectorAll('.thread').length").await,
        1
    );
    assert_eq!(eval::<u32>(page, "window.__rejections").await, 0);
    tap_line(page, 8).await;
    assert_eq!(
        eval::<u32>(page, "document.querySelectorAll('.newbox').length").await,
        1
    );
    rig.close().await;
}
