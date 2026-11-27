//! Browser launch shared by the suites in this directory (a directory module, so it is not a
//! test binary). Each launch gets a Chrome profile of its own under `dir`, so tests run in
//! parallel without fighting over Chrome's `SingletonLock`.

use std::path::Path;
use std::sync::Arc;

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;
use margin::agents::OsRng;
use margin::api::{AppState, router};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin::store::{Kind, NewCollection, NewDocument};
use margin_core::SystemClock;

/// The 3,000-line file every highlight suite serves.
#[allow(dead_code)]
pub const FIXTURE: &str = include_str!("../../fixtures/long.rs");

#[allow(dead_code)]
pub async fn launch(dir: &Path) -> (Browser, tokio::task::JoinHandle<()>) {
    let chromium = std::env::var("MARGIN_CHROMIUM").expect(
        "MARGIN_CHROMIUM must name a chromium binary; this suite refuses to run without it",
    );
    let config = BrowserConfig::builder()
        .chrome_executable(chromium)
        .no_sandbox()
        .user_data_dir(dir.join("chrome-profile"))
        .args([
            "--disable-gpu",
            "--disable-dev-shm-usage",
            "--hide-scrollbars",
        ])
        .build()
        .unwrap();
    let (browser, mut handler) = Browser::launch(config).await.unwrap();
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });
    (browser, pump)
}

/// Serves the fixture collection in-process; returns its page URL and the directory that holds its database.
#[allow(dead_code)]
pub async fn serve() -> (String, tempfile::TempDir) {
    serve_docs("Fixture", &[("long.rs", FIXTURE.to_string())], "").await
}

/// Serves one collection, `fixture`, of the given (path, text) documents; `agents` is the
/// agents file's text. Returns its page URL and the directory that holds its database.
#[allow(dead_code)]
pub async fn serve_docs(
    title: &str,
    documents: &[(&str, String)],
    agents_toml: &str,
) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents.toml");
    std::fs::write(&agents, agents_toml).unwrap();
    let config = Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\nbase_url = \"http://127.0.0.1\"\ndatabase = \"{}\"\nagents_file = \"{}\"\notlp_endpoint = \"http://127.0.0.1:1\"\ninstance = \"test\"\npage_lines = 1500\n",
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
    let title = title.to_string();
    let documents: Vec<(String, NewDocument)> = documents
        .iter()
        .enumerate()
        .map(|(i, (path, text))| {
            (
                format!("d{}", i + 1),
                NewDocument {
                    path: (*path).into(),
                    kind: Kind::Code,
                    text: text.clone(),
                },
            )
        })
        .collect();
    state
        .db
        .call(move |conn| {
            margin::store::create(
                conn,
                &NewCollection {
                    id: "fixture",
                    title: &title,
                    cover_note: "A long file.",
                    created_by: "ui",
                    now: 0,
                    source: None,
                    documents: &documents,
                },
            )
        })
        .await
        .unwrap()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/c/fixture", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (url, dir)
}

/// Script that scrolls line `first` to the middle and selects columns `c1` of `first` through
/// `c2` of `last`; it evaluates to `true`.
#[allow(dead_code)]
pub fn select_js(first: u32, c1: u32, last: u32, c2: u32) -> String {
    select_in_js("", first, c1, last, c2)
}

/// `select_js` within the elements `scope` names (a selector prefix such as `#doc-2 `).
#[allow(dead_code)]
pub fn select_in_js(scope: &str, first: u32, c1: u32, last: u32, c2: u32) -> String {
    format!(
        "(() => {{
          const at = (n, col) => {{
            const code = document.querySelector('{scope}[data-line=\"' + n + '\"] .code');
            const walker = document.createTreeWalker(code, NodeFilter.SHOW_TEXT);
            let left = col;
            for (let t = walker.nextNode(); t; t = walker.nextNode()) {{
              if (left <= t.data.length) return [t, left];
              left -= t.data.length;
            }}
            throw new Error('column past the line');
          }};
          document.querySelector('{scope}[data-line=\"{first}\"]').scrollIntoView({{block: 'center'}});
          const [a, ao] = at({first}, {c1});
          const [b, bo] = at({last}, {c2});
          const r = document.createRange();
          r.setStart(a, ao);
          r.setEnd(b, bo);
          const s = getSelection();
          s.removeAllRanges();
          s.addRange(r);
          return true;
        }})()"
    )
}
