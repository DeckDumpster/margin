//! Drives a real Chromium over CDP against an in-process server. Run by `gate.sh`
//! with `-- --ignored`; refuses by name when `MARGIN_CHROMIUM` is unset.

use std::path::PathBuf;
use std::sync::Arc;

use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::page::{Page, ScreenshotParams};
use margin::agents::OsRng;
use margin::api::{AppState, router};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin::store::{Kind, NewCollection, NewDocument};
use margin_core::SystemClock;
use margin_test_id::test_id;

mod support;

const FIXTURE: &str = include_str!("../fixtures/long.rs");
const PHONE: u32 = 390;

#[derive(Debug, serde::Deserialize)]
struct Widths {
    inner_width: u32,
    page_scroll_width: u32,
    outside_frame_overflow: Vec<String>,
    any_overflow: Vec<String>,
    frame_scroll_width: u32,
    frame_client_width: u32,
    data_lines: u32,
    nowrap_class: bool,
    stored: Option<String>,
}

const MEASURE: &str = r#"(() => {
  const W = 390;
  const over = (el) => el.getBoundingClientRect().right > W + 0.5;
  const label = (el) => el.tagName.toLowerCase() + '.' + el.className + ' right=' + el.getBoundingClientRect().right;
  const all = [...document.body.querySelectorAll('*')];
  const frame = document.querySelector('.frame');
  let stored = null;
  try { stored = localStorage.getItem('margin.wrap'); } catch (e) {}
  return {
    inner_width: window.innerWidth,
    page_scroll_width: document.documentElement.scrollWidth,
    outside_frame_overflow: all.filter((el) => !el.closest('.frame') && over(el)).map(label),
    any_overflow: all.filter(over).slice(0, 5).map(label),
    frame_scroll_width: frame.scrollWidth,
    frame_client_width: frame.clientWidth,
    data_lines: document.querySelectorAll('[data-line]').length,
    nowrap_class: document.documentElement.classList.contains('nowrap'),
    stored,
  };
})()"#;

async fn widths(page: &Page) -> Widths {
    page.evaluate(MEASURE).await.unwrap().into_value().unwrap()
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

fn shot_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Evidence only: nothing asserts on these.
async fn screenshot(page: &Page, name: &str) {
    // Bring the long lines of the fixture's first lines into view, then capture the viewport.
    page.evaluate("document.querySelector('[data-line=\"31\"]').scrollIntoView({block: 'start'})")
        .await
        .unwrap();
    page.save_screenshot(ScreenshotParams::builder().build(), shot_dir().join(name))
        .await
        .unwrap();
    page.evaluate("window.scrollTo(0, 0)").await.unwrap();
}

async fn serve() -> (String, tempfile::TempDir) {
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

#[test_id(t4ttegj)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn the_page_never_scrolls_sideways_on_a_phone_and_remembers_the_wrap_choice() {
    let (url, dir) = serve().await;
    let (mut browser, pump) = support::launch(dir.path()).await;

    let page = browser.new_page("about:blank").await.unwrap();
    viewport(&page, PHONE, 844, true).await;
    page.goto(&url)
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();

    // Wrap on (the default).
    let on = widths(&page).await;
    assert_eq!(on.data_lines, 3000, "{on:?}");
    assert!(!on.nowrap_class, "{on:?}");
    assert!(
        on.page_scroll_width <= PHONE,
        "wrap on: page scrolls sideways: {on:?}"
    );
    assert!(
        on.any_overflow.is_empty(),
        "wrap on: elements past {PHONE}px: {on:?}"
    );
    assert_eq!(
        on.inner_width, PHONE,
        "the phone viewport was not applied: {on:?}"
    );
    screenshot(&page, "phone-390-wrap-on.png").await;

    // Wrap off.
    page.find_element("#wrap-toggle")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    let off = widths(&page).await;
    assert!(off.nowrap_class, "{off:?}");
    assert!(
        off.page_scroll_width <= PHONE,
        "wrap off: page scrolls sideways: {off:?}"
    );
    assert!(
        off.outside_frame_overflow.is_empty(),
        "wrap off: elements outside the frame past {PHONE}px: {off:?}"
    );
    assert!(
        off.frame_scroll_width > off.frame_client_width,
        "wrap off: the frame does not scroll: {off:?}"
    );
    screenshot(&page, "phone-390-wrap-off.png").await;

    // The device remembers it across a reload.
    page.reload()
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();
    let again = widths(&page).await;
    assert!(again.nowrap_class, "wrap choice lost on reload: {again:?}");
    assert_eq!(again.stored.as_deref(), Some("off"));
    assert!(again.page_scroll_width <= PHONE, "{again:?}");
    assert!(
        again.frame_scroll_width > again.frame_client_width,
        "{again:?}"
    );

    // Desktop width, for the record.
    viewport(&page, 1280, 900, false).await;
    screenshot(&page, "desktop-1280-wrap-off.png").await;
    page.find_element("#wrap-toggle")
        .await
        .unwrap()
        .click()
        .await
        .unwrap();
    screenshot(&page, "desktop-1280-wrap-on.png").await;

    browser.close().await.unwrap();
    let _ = pump.await;
    browser.wait().await.unwrap();
}
