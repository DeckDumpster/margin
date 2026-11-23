//! A new version in place: threads carried, removed and text-changed, changed lines marked.
//! Run by `gate.sh` with `-- --ignored`; refuses by name when `MARGIN_CHROMIUM` is unset.
//! Screenshots at 390 and 1280 are evidence; the width assertions are draft 1's.

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

fn original() -> String {
    (1..=40)
        .map(|n| format!("fn line_{n}() {{ let value = {n}; }}\n"))
        .collect()
}

/// Lines 10 and 11 deleted, line 30 rewritten, three lines inserted at the top.
fn revised() -> String {
    let mut out: String = (1..=3).map(|n| format!("// inserted {n}\n")).collect();
    for n in (1..=40).filter(|n| *n != 10 && *n != 11) {
        if n == 30 {
            out.push_str("fn line_30() { let value = \"rewritten\"; }\n");
        } else {
            out.push_str(&format!("fn line_{n}() {{ let value = {n}; }}\n"));
        }
    }
    out
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
            let docs = vec![(
                "d1".to_string(),
                NewDocument {
                    path: "a.rs".into(),
                    kind: Kind::Code,
                    text: original(),
                },
            )];
            margin::store::create(
                conn,
                &NewCollection {
                    id: "versions",
                    title: "Versions",
                    cover_note: "",
                    created_by: "ui",
                    now: 0,
                    documents: &docs,
                },
            )
            .unwrap();
            let version_id: i64 = conn
                .query_row("SELECT id FROM versions", [], |r| r.get(0))
                .unwrap();
            // Carried (line 20), removed (10-11), text-changed highlight (30).
            for (id, anchor) in [
                ("t-carried", Anchor::lines(20, 20)),
                ("t-removed", Anchor::lines(10, 11)),
                (
                    "t-changed",
                    Anchor {
                        first: 30,
                        last: 30,
                        cols: Some((4, 10)),
                    },
                ),
            ] {
                threads::create_thread(
                    conn,
                    &NewThread {
                        thread_id: id,
                        comment_id: &format!("c-{id}"),
                        collection_id: "versions",
                        version_id,
                        anchor,
                        author: "agent",
                        visibility: Visibility::Now,
                        body: id,
                        now: 1,
                    },
                )
                .unwrap_or_else(|_| panic!("thread {id}"));
            }
            margin::versions::add(conn, "d1", &revised(), 2)
                .unwrap()
                .unwrap();
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/c/versions", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    (url, dir)
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

#[derive(Debug, serde::Deserialize)]
struct Seen {
    version_label: String,
    changed_lines: Vec<u32>,
    labels: Vec<(String, u32)>,
    page_scroll_width: u32,
    any_overflow: Vec<String>,
}

fn measure(width: u32) -> String {
    format!(
        r#"(() => {{
  const W = {width};
  const over = (el) => el.getBoundingClientRect().right > W + 0.5;
  const all = [...document.body.querySelectorAll('*')];
  const labels = [...document.querySelectorAll('.thread .carry')].map((el) => {{
    const row = el.closest('.thread').previousElementSibling;
    const line = row.matches('.ln') ? row : row.previousElementSibling;
    return [el.textContent, Number(line.dataset.line)];
  }});
  return {{
    version_label: document.querySelector('.version').textContent,
    changed_lines: [...document.querySelectorAll('.ln.changed')].map((el) => Number(el.dataset.line)),
    labels,
    page_scroll_width: document.documentElement.scrollWidth,
    any_overflow: all.filter(over).slice(0, 5).map((el) => el.tagName + '.' + el.className),
  }};
}})()"#
    )
}

#[test_id(t5w4trt)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_carried_a_removed_and_a_text_changed_thread_show_with_changed_lines_marked() {
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
    let shots = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&shots).unwrap();

    for (width, height, mobile) in [(390, 844, true), (1280, 900, false)] {
        viewport(&page, width, height, mobile).await;
        page.goto(&url)
            .await
            .unwrap()
            .wait_for_navigation()
            .await
            .unwrap();
        // Open every thread so its label shows in the picture.
        page.evaluate("document.querySelectorAll('.thread').forEach((t) => t.hidden = false)")
            .await
            .unwrap();
        let seen: Seen = page
            .evaluate(measure(width))
            .await
            .unwrap()
            .into_value()
            .unwrap();
        assert_eq!(seen.version_label, "version 2", "{seen:?}");
        assert_eq!(seen.changed_lines, [1, 2, 3, 31], "{seen:?}");
        let mut labels = seen.labels.clone();
        labels.sort_by_key(|l| l.1);
        assert_eq!(
            labels,
            [
                ("on a line since removed".to_string(), 9 + 3),
                ("text since changed".to_string(), 31),
            ],
            "{seen:?}"
        );
        assert!(
            seen.page_scroll_width <= width,
            "page scrolls sideways: {seen:?}"
        );
        if width == 390 {
            assert!(
                seen.any_overflow.is_empty(),
                "elements past {width}px: {seen:?}"
            );
        }
        page.evaluate(
            "document.querySelector('[data-line=\"1\"]').scrollIntoView({block: 'start'})",
        )
        .await
        .unwrap();
        page.save_screenshot(
            ScreenshotParams::builder().build(),
            shots.join(format!("versions-{width}.png")),
        )
        .await
        .unwrap();
    }
    browser.close().await.unwrap();
    let _ = pump.await;
}
