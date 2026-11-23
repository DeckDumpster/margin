//! The home page at phone size with long titles, notes and agent names. Run by `gate.sh` with
//! `-- --ignored`; refuses by name when `MARGIN_CHROMIUM` is unset.

use std::path::PathBuf;
use std::sync::Arc;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;
use margin::agents::OsRng;
use margin::api::{AppState, router};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin::store::{Kind, NewCollection, NewDocument};
use margin::threads::{self, NewThread, Visibility};
use margin_core::SystemClock;
use margin_core::anchor::Anchor;
use margin_test_id::test_id;

const PHONE: u32 = 390;

fn shot_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn viewport(page: &Page, width: u32, height: u32, mobile: bool) {
    page.execute(SetDeviceMetricsOverrideParams::new(
        width as i64,
        height as i64,
        1.0,
        mobile,
    ))
    .await
    .unwrap();
}

async fn serve() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents.toml");
    std::fs::write(&agents, "").unwrap();
    let config = Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\nbase_url = \"http://127.0.0.1\"\ndatabase = \"{}\"\nagents_file = \"{}\"\notlp_endpoint = \"http://127.0.0.1:1\"\n",
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
            let long_title = "A very long collection title with no end in sight ".repeat(4);
            let unbroken = "x".repeat(120);
            let long_agent = "an-agent-with-an-extremely-long-name-that-keeps-going-and-going";
            for (n, title) in [long_title.as_str(), unbroken.as_str(), "Short"]
                .into_iter()
                .enumerate()
            {
                let id = format!("c{n}");
                let docs = vec![(
                    format!("d{n}"),
                    NewDocument {
                        path: "a.txt".into(),
                        kind: Kind::Code,
                        text: "a\nb\n".into(),
                    },
                )];
                margin::store::create(
                    conn,
                    &NewCollection {
                        id: &id,
                        title,
                        cover_note: &format!("{unbroken} cover note {long_title}"),
                        created_by: long_agent,
                        now: 100 + n as i64,
                        documents: &docs,
                    },
                )
                .unwrap();
                let version =
                    margin::store::get(conn, &id).unwrap().unwrap().documents[0].version_id;
                for (t, author) in [("a", long_agent), ("b", "reader")] {
                    threads::create_thread(
                        conn,
                        &NewThread {
                            thread_id: &format!("{id}{t}"),
                            comment_id: &format!("{id}{t}-0"),
                            collection_id: &id,
                            version_id: version,
                            anchor: Anchor {
                                first: 1,
                                last: 1,
                                cols: None,
                            },
                            author,
                            visibility: Visibility::Now,
                            body: "hello",
                            now: 200 + n as i64,
                        },
                    )
                    .ok()
                    .unwrap();
                }
            }
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (url, dir)
}

#[test_id(t4gaalt)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn the_home_page_never_scrolls_sideways_on_a_phone_with_long_titles_and_agent_names() {
    let chromium = std::env::var("MARGIN_CHROMIUM").expect(
        "MARGIN_CHROMIUM must name a chromium binary; this suite refuses to run without it",
    );
    let (url, dir) = serve().await;
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
    let (mut browser, mut handler) = Browser::launch(config).await.unwrap();
    let pump = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.unwrap();
    viewport(&page, PHONE, 844, true).await;
    page.goto(&url)
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();

    let entries: i64 = page
        .evaluate("document.querySelectorAll('a.entry').length")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(entries, 3);
    let inner: i64 = page
        .evaluate("window.innerWidth")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(inner, PHONE as i64, "the phone viewport was not applied");
    let scroll: i64 = page
        .evaluate("document.documentElement.scrollWidth")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(
        scroll <= PHONE as i64,
        "the home page scrolls sideways: {scroll}"
    );
    page.save_screenshot(
        ScreenshotParams::builder().build(),
        shot_dir().join("home-390.png"),
    )
    .await
    .unwrap();

    viewport(&page, 1280, 900, false).await;
    page.save_screenshot(
        ScreenshotParams::builder().build(),
        shot_dir().join("home-1280.png"),
    )
    .await
    .unwrap();

    browser.close().await.unwrap();
    let _ = pump.await;
}
