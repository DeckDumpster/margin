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
