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

pub mod webkit;

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
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents.toml");
    std::fs::write(&agents, "").unwrap();
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
    state
        .db
        .call(|conn| {
            let docs = vec![(
                "d1".to_string(),
                NewDocument {
                    path: "long.rs".into(),
                    kind: Kind::Code,
                    text: FIXTURE.to_string(),
                },
            )];
            margin::store::create(
                conn,
                &NewCollection {
                    id: "fixture",
                    title: "Fixture",
                    cover_note: "A long file.",
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
    let url = format!("http://{}/c/fixture", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (url, dir)
}
