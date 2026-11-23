//! Drives a real Chromium over CDP against an in-process server. Run by `gate.sh`
//! with `-- --ignored`; refuses by name when `MARGIN_CHROMIUM` is unset.

use std::path::PathBuf;
use std::sync::Arc;

use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
};
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
    serve_with_cover("A long file.").await
}

async fn serve_with_cover(cover: &str) -> (String, tempfile::TempDir) {
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
    let cover = cover.to_string();
    state
        .db
        .call(move |conn| {
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
                    cover_note: &cover,
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

const CODE_STYLE: &str = r#"(() => {
  const sides = ['Top', 'Right', 'Bottom', 'Left'];
  const bad = new Map();
  let spans = 0;
  for (const code of document.querySelectorAll('.code')) {
    const want = getComputedStyle(code);
    for (const span of code.querySelectorAll('span')) {
      spans++;
      const got = getComputedStyle(span);
      const margins = sides.map((s) => got['margin' + s]);
      const ok = got.fontSize === want.fontSize && got.fontWeight === want.fontWeight
        && got.lineHeight === want.lineHeight && margins.every((m) => m === '0px');
      if (!ok) {
        const key = span.className + ' ' + got.fontSize + '/' + got.fontWeight + '/' + got.lineHeight + ' margin ' + margins.join(' ');
        bad.set(key, (bad.get(key) || 0) + 1);
      }
    }
  }
  const code = document.querySelector('.code');
  const colour = (sel) => {
    const el = [...document.querySelectorAll('.code ' + sel)][0];
    return el ? getComputedStyle(el).color : null;
  };
  const comment = document.querySelector('.code .tok-comment');
  return {
    spans,
    offenders: [...bad].slice(0, 5).map(([k, n]) => k + ' x' + n),
    path_spans: document.querySelectorAll('.code .tok-path').length,
    code_colour: getComputedStyle(code).color,
    keyword_colour: colour('.tok-keyword, .tok-storage'),
    comment_style: comment ? getComputedStyle(comment).fontStyle : null,
  };
})()"#;

#[derive(Debug, serde::Deserialize)]
struct CodeStyle {
    spans: u32,
    offenders: Vec<String>,
    path_spans: u32,
    code_colour: String,
    keyword_colour: Option<String>,
    comment_style: Option<String>,
}

#[test_id(tfre3l9)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn code_tokens_have_their_lines_font_and_no_margin_whatever_their_scope() {
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
    let s: CodeStyle = page
        .evaluate(CODE_STYLE)
        .await
        .unwrap()
        .into_value()
        .unwrap();

    assert!(s.spans > 0, "{s:?}");
    assert!(
        s.offenders.is_empty(),
        "spans that differ from their .code (font-size/weight/line-height, margin): {:#?}",
        s.offenders
    );
    assert!(s.path_spans > 0, "no .tok-path span to test: {s:?}");
    let keyword = s.keyword_colour.as_deref();
    assert!(
        keyword.is_some_and(|k| k != s.code_colour),
        "keywords are not coloured: {s:?}"
    );
    assert_eq!(s.comment_style.as_deref(), Some("italic"), "{s:?}");

    browser.close().await.unwrap();
    let _ = pump.await;
    browser.wait().await.unwrap();
}

#[test_id(tvngowa)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_cover_note_with_wide_code_and_a_table_never_scrolls_sideways_on_a_phone() {
    let span = "x".repeat(120);
    let block = "y".repeat(160);
    let cover = format!(
        "## Head\n\nA `{span}` span.\n\n```\n{block}\n```\n\n| first column | second column | third column | fourth column |\n|---|---|---|---|\n| aaaaaaaaaaaaaaaa | bbbbbbbbbbbbbbbb | cccccccccccccccc | dddddddddddddddd |\n"
    );
    let (url, dir) = serve_with_cover(&cover).await;
    let (mut browser, pump) = support::launch(dir.path()).await;
    let page = browser.new_page("about:blank").await.unwrap();
    viewport(&page, PHONE, 844, true).await;
    page.goto(&url)
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();
    let over: Vec<String> = page
        .evaluate(
            "[...document.querySelectorAll('.cover, .cover *')].filter(e => e.getBoundingClientRect().right > 390.5).map(e => e.tagName)",
        )
        .await
        .unwrap()
        .into_value()
        .unwrap();
    let tables: u32 = page
        .evaluate("document.querySelectorAll('.cover table').length")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert_eq!(tables, 1);
    assert!(over.is_empty(), "past the viewport: {over:?}");
    let sw: u32 = page
        .evaluate("document.documentElement.scrollWidth")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(sw <= PHONE, "page scrolls sideways: {sw}");
    browser.close().await.unwrap();
    let _ = pump.await;
}

/// Evidence only: nothing asserts on these. The cover of a `cr` description, as the reader sees it.
#[test_id(t4qr6zi)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn the_cover_note_is_photographed_on_a_phone_and_a_desktop_in_both_schemes() {
    let cover = "## Summary\n\nThe review page renders its cover note.\n\n- headings and lists\n  - nested items with `code spans`\n- a [link](https://example.com) and **bold** text\n\nSource repository: margin\nSource commit: 4c00ebb\n\n```\nfn render(text: &str) -> String { markdown::html(text) }\n```\n\n| field | before | after |\n|---|---|---|\n| cover | source | markup |\n\nWorld<T> stays text.";
    let (url, dir) = serve_with_cover(cover).await;
    let (mut browser, pump) = support::launch(dir.path()).await;
    let page = browser.new_page("about:blank").await.unwrap();
    for (width, mobile) in [(390, true), (1280, false)] {
        for scheme in ["light", "dark"] {
            viewport(&page, width, 700, mobile).await;
            page.execute(
                SetEmulatedMediaParams::builder()
                    .feature(MediaFeature::new("prefers-color-scheme", scheme))
                    .build(),
            )
            .await
            .unwrap();
            page.goto(&url)
                .await
                .unwrap()
                .wait_for_navigation()
                .await
                .unwrap();
            let name = format!("cover-after-{width}-{scheme}.png");
            page.save_screenshot(ScreenshotParams::builder().build(), shot_dir().join(name))
                .await
                .unwrap();
        }
    }
    browser.close().await.unwrap();
    let _ = pump.await;
}
