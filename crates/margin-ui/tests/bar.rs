//! The sticky bar: once the page's heading has scrolled away, a bar stays at the top with the
//! review's title, the current file's name and Top, previous-file and next-file controls. Driven
//! at phone size over three documents, the last of three lines. Run by `gate.sh` with
//! `-- --ignored`; refuses by name when `MARGIN_CHROMIUM` is unset. Waits are on signals from the
//! page, never on elapsed time. Screenshots are evidence only.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::emulation::{
    MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams,
    SetTouchEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchTouchEventParams, DispatchTouchEventReturns, DispatchTouchEventType, TimeSinceEpoch,
    TouchPoint,
};
use chromiumoxide::page::{Page, ScreenshotParams};
use margin::agents::hash_token;
use margin_test_id::test_id;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod support;

const PHONE: u32 = 390;
const HEIGHT: u32 = 844;
const TOKEN: &str = "bar-token-0123456789";
const SIGNAL_DEADLINE: Duration = Duration::from_secs(20);

/// `n` lines of `fn <name>_<i>() {}`.
fn lines(name: &str, n: u32) -> String {
    (1..=n).map(|i| format!("fn {name}_{i}() {{}}\n")).collect()
}

struct Rig {
    browser: Browser,
    page: Page,
    pump: tokio::task::JoinHandle<()>,
    addr: String,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    rig_with(
        "Review",
        ["src/deep/a.rs", "src/b.rs", "c.rs"].map(String::from),
    )
    .await
}

async fn rig_with(title: &str, paths: [String; 3]) -> Rig {
    let docs = [
        (paths[0].as_str(), lines("a", 150)),
        (paths[1].as_str(), lines("b", 150)),
        (paths[2].as_str(), lines("c", 3)),
    ];
    let (url, dir) =
        support::serve_docs(title, &docs, &format!("ada = \"{}\"\n", hash_token(TOKEN))).await;
    let addr = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap()
        .to_string();
    let (browser, pump) = support::launch(dir.path()).await;
    let page = browser.new_page("about:blank").await.unwrap();
    viewport(&page, PHONE, HEIGHT, true).await;
    page.execute(SetTouchEmulationEnabledParams::new(true))
        .await
        .unwrap();
    page.goto(&url)
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
        self.browser.wait().await.unwrap();
    }

    /// One agent request over a plain socket; returns the status.
    async fn agent(&self, method: &str, path: &str, body: serde_json::Value) -> u16 {
        let body = body.to_string();
        let mut sock = tokio::net::TcpStream::connect(&self.addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nhost: margin\r\nauthorization: Bearer {TOKEN}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        sock.read_to_end(&mut raw).await.unwrap();
        String::from_utf8(raw)
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap()
    }
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

const BAR_SHOWN: &str = "(() => { const b = document.querySelector('.bar'); return !!b && !b.hidden && b.getBoundingClientRect().height > 0; })()";
const BAR_BOTTOM: &str = "document.querySelector('.bar').getBoundingClientRect().bottom";

async fn scroll_to(page: &Page, y: u32) {
    eval::<bool>(page, &format!("(window.scrollTo(0, {y}), true)")).await;
}

async fn bar_file(page: &Page) -> String {
    eval(page, "document.getElementById('bar-file').textContent").await
}

/// Press a bar control the way a tap does, and return once the page has handled the click.
async fn press(page: &Page, id: &str) {
    eval::<bool>(
        page,
        &format!("(document.getElementById('{id}').click(), true)"),
    )
    .await;
}

/// Scroll position has stopped moving (smooth or not): the page's own frame signal.
async fn settle(page: &Page) {
    eval::<bool>(
        page,
        "new Promise((done) => { let last = -1, same = 0; const f = () => { same = scrollY === last ? same + 1 : 0; last = scrollY; same >= 3 ? done(true) : requestAnimationFrame(f); }; f(); })",
    )
    .await;
}

fn shot_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Evidence only: nothing asserts on these.
async fn screenshot(page: &Page, name: &str) {
    page.save_screenshot(ScreenshotParams::builder().build(), shot_dir().join(name))
        .await
        .unwrap();
}

async fn colour_scheme(page: &Page, scheme: &str) {
    let params = SetEmulatedMediaParams {
        features: Some(vec![MediaFeature::new("prefers-color-scheme", scheme)]),
        ..Default::default()
    };
    page.execute(params).await.unwrap();
}

#[derive(serde::Serialize)]
struct EndTouch {
    #[serde(rename = "type")]
    kind: DispatchTouchEventType,
    #[serde(rename = "touchPoints")]
    touch_points: Vec<TouchPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<TimeSinceEpoch>,
}

impl chromiumoxide::Method for EndTouch {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        "Input.dispatchTouchEvent".into()
    }
}

impl chromiumoxide::Command for EndTouch {
    type Response = DispatchTouchEventReturns;
}

async fn touch(page: &Page, kind: DispatchTouchEventType, at: Option<(f64, f64)>) {
    if matches!(kind, DispatchTouchEventType::TouchEnd) {
        page.execute(EndTouch {
            kind,
            touch_points: Vec::new(),
            timestamp: None,
        })
        .await
        .unwrap();
        return;
    }
    let points = at
        .map(|(x, y)| vec![TouchPoint::new(x, y)])
        .unwrap_or_default();
    page.execute(DispatchTouchEventParams::new(kind, points))
        .await
        .unwrap();
}

#[test_id(tetfjxa)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn the_bar_is_hidden_at_the_top_shown_once_the_heading_scrolls_away_and_top_returns() {
    let rig = rig().await;
    let page = &rig.page;
    assert!(
        !eval::<bool>(page, BAR_SHOWN).await,
        "the bar shows at the top"
    );
    scroll_to(page, 600).await;
    wait_for(page, "the bar showed", BAR_SHOWN).await;
    let (top, title): (f64, String) = eval(
        page,
        "(() => { const b = document.querySelector('.bar'); return [b.getBoundingClientRect().top, b.querySelector('.bar-title').textContent]; })()",
    )
    .await;
    assert_eq!(top, 0.0, "the bar sits at the top of the screen");
    assert_eq!(title, "Review");
    press(page, "bar-top").await;
    wait_for(page, "the page is at the top", "window.scrollY === 0").await;
    wait_for(page, "the bar went", &format!("!({BAR_SHOWN})")).await;
    rig.close().await;
}

#[test_id(ttt4skm)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn next_visits_every_file_names_each_then_is_disabled_and_previous_returns() {
    let rig = rig().await;
    let page = &rig.page;
    // Just past the heading: the bar is up, and the first file is still below it.
    eval::<bool>(
        page,
        "(window.scrollTo(0, document.querySelector('header.top').getBoundingClientRect().bottom + 1), true)",
    )
    .await;
    wait_for(page, "the bar showed", BAR_SHOWN).await;
    assert_eq!(bar_file(page).await, "", "no file before the first");
    assert!(
        eval::<bool>(page, "document.getElementById('bar-prev').disabled").await,
        "no previous before the first file"
    );

    let jumped = |n: u32| {
        format!(
            "(() => {{ const t = document.getElementById('doc-{n}').getBoundingClientRect().top; return t >= {BAR_BOTTOM} - 1 && t <= {BAR_BOTTOM} + 1; }})()"
        )
    };
    for (n, name) in [(1, "a.rs"), (2, "b.rs"), (3, "c.rs")] {
        press(page, "bar-next").await;
        settle(page).await;
        wait_for(page, "the jump landed", &jumped(n)).await;
        wait_for(
            page,
            "the bar named the file",
            &format!("document.getElementById('bar-file').textContent === '{name}'"),
        )
        .await;
    }
    let title: String = eval(page, "document.getElementById('bar-file').title").await;
    assert_eq!(title, "c.rs");
    assert!(
        eval::<bool>(page, "document.getElementById('bar-next').disabled").await,
        "no next past the last file"
    );

    for (n, name) in [(2, "b.rs"), (1, "a.rs")] {
        press(page, "bar-prev").await;
        settle(page).await;
        wait_for(page, "the jump landed", &jumped(n)).await;
        wait_for(
            page,
            "the bar named the file",
            &format!("document.getElementById('bar-file').textContent === '{name}'"),
        )
        .await;
    }
    let full: String = eval(page, "document.getElementById('bar-file').title").await;
    assert_eq!(
        full, "src/deep/a.rs",
        "the bar shows the basename, the path in its title"
    );
    rig.close().await;
}

#[test_id(twana6u)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_held_range_dragged_to_the_top_edge_scrolls_up_and_extends_past_the_first_screen() {
    let rig = rig().await;
    let page = &rig.page;
    scroll_to(page, 600).await;
    wait_for(page, "the bar showed", BAR_SHOWN).await;
    let (x, y): (f64, f64) = eval(
        page,
        "(() => { const row = document.querySelector('#doc-2 [data-line=\"100\"]');
          row.scrollIntoView({block: 'center'});
          const r = row.querySelector('.no').getBoundingClientRect();
          return [r.left + r.width / 2, r.top + r.height / 2]; })()",
    )
    .await;
    let top_visible = format!(
        "Math.min(...[...document.querySelectorAll('#doc-2 [data-line]')].filter((r) => r.getBoundingClientRect().bottom > {BAR_BOTTOM}).map((r) => Number(r.dataset.line)))"
    );
    let first_screen_top: u32 = eval(page, &top_visible).await;
    let scrolled: f64 = eval(page, "window.scrollY").await;

    touch(page, DispatchTouchEventType::TouchStart, Some((x, y))).await;
    wait_for(
        page,
        "the hold began a range",
        "document.body.dataset.gesture === 'ranging'",
    )
    .await;
    // The finger goes right over the bar.
    touch(page, DispatchTouchEventType::TouchMove, Some((x, y / 2.0))).await;
    touch(page, DispatchTouchEventType::TouchMove, Some((x, 10.0))).await;
    wait_for(
        page,
        "the page scrolled up by itself",
        &format!("window.scrollY < {}", scrolled - 200.0),
    )
    .await;
    wait_for(
        page,
        "the range passed the first screen",
        &format!(
            "[...document.querySelectorAll('.ln.sel')].some((r) => Number(r.dataset.line) < {})",
            first_screen_top.saturating_sub(15)
        ),
    )
    .await;
    touch(page, DispatchTouchEventType::TouchEnd, None).await;
    wait_for(page, "the box opened", "document.querySelector('.newbox')").await;
    let (first, last): (u32, u32) = eval(
        page,
        "[Number(document.querySelector('.newbox').dataset.first), Number(document.querySelector('.newbox').dataset.last)]",
    )
    .await;
    assert_eq!(last, 100, "the hold's line stays the far end");
    assert!(
        first < first_screen_top.saturating_sub(15),
        "range starts at {first}, the first screen began at {first_screen_top}"
    );
    rig.close().await;
}

#[test_id(tld6lp3)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn the_comment_button_on_scrolled_selected_words_clears_the_bar_and_opens_the_box() {
    let rig = rig().await;
    let page = &rig.page;
    scroll_to(page, 600).await;
    wait_for(page, "the bar showed", BAR_SHOWN).await;
    eval::<bool>(page, &support::select_in_js("#doc-2 ", 60, 3, 60, 10)).await;
    // Bring the selection's end up under the bar, where the button used to be able to land.
    eval::<bool>(
        page,
        &format!(
            "(() => {{ const rs = getSelection().getRangeAt(0).getClientRects(); const end = rs[rs.length - 1]; scrollBy(0, end.bottom - {BAR_BOTTOM} - 4); return true; }})()"
        ),
    )
    .await;
    wait_for(
        page,
        "the Comment button showed",
        "(() => { const b = document.getElementById('hl-comment'); return b && !b.hidden; })()",
    )
    .await;
    wait_for(
        page,
        "the button is clear of the bar",
        &format!(
            "document.getElementById('hl-comment').getBoundingClientRect().top >= {BAR_BOTTOM}"
        ),
    )
    .await;
    press(page, "hl-comment").await;
    wait_for(
        page,
        "the box opened with columns",
        "document.querySelector('.newbox[data-first=\"60\"][data-start-col=\"3\"]')",
    )
    .await;
    rig.close().await;
}

#[test_id(tg9qk0c)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_live_version_and_title_change_keep_the_bar_right() {
    let rig = rig().await;
    let page = &rig.page;
    eval::<bool>(
        page,
        "(document.getElementById('doc-2').scrollIntoView(), true)",
    )
    .await;
    wait_for(
        page,
        "the bar names the second file",
        "document.getElementById('bar-file').textContent === 'b.rs'",
    )
    .await;

    let status = rig
        .agent(
            "POST",
            "/api/documents/d2/versions",
            json!({"text": lines("b2", 150)}),
        )
        .await;
    assert_eq!(status, 201);
    wait_for(
        page,
        "the new version label",
        "document.querySelector('#doc-2 .version').textContent === 'version 2'",
    )
    .await;
    let status = rig
        .agent(
            "PATCH",
            "/api/collections/fixture",
            json!({"title": "Renamed review"}),
        )
        .await;
    assert_eq!(status, 200);
    wait_for(
        page,
        "the bar shows the new title",
        "document.querySelector('.bar-title').textContent === 'Renamed review'",
    )
    .await;
    assert_eq!(bar_file(page).await, "b.rs");
    let (title, tab): (String, String) = eval(
        page,
        "[document.getElementById('bar-file').title, document.title]",
    )
    .await;
    assert_eq!(title, "src/b.rs");
    assert_eq!(tab, "Renamed review · margin");
    rig.close().await;
}

#[test_id(t6qcyd1)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_long_title_and_file_name_leave_no_element_past_the_screen_edge() {
    let long_path = format!("src/{}.rs", "x".repeat(150));
    let rig = rig_with(
        &"Very long review title ".repeat(12),
        [long_path, "b.rs".into(), "c.rs".into()],
    )
    .await;
    let page = &rig.page;
    eval::<bool>(
        page,
        "(document.getElementById('doc-1').scrollIntoView(), true)",
    )
    .await;
    wait_for(page, "the bar showed", BAR_SHOWN).await;
    wait_for(
        page,
        "the bar names the file",
        "document.getElementById('bar-file').textContent.length > 100",
    )
    .await;
    let (width, over): (u32, Vec<String>) = eval(
        page,
        &format!(
            "(() => [document.documentElement.scrollWidth, [...document.body.querySelectorAll('*')].filter((el) => !el.closest('.frame') && el.getBoundingClientRect().right > {PHONE} + 0.5).map((el) => el.tagName.toLowerCase() + '.' + el.className).slice(0, 5)])()"
        ),
    )
    .await;
    assert!(width <= PHONE, "page scrolls sideways: {width}");
    assert!(over.is_empty(), "elements past {PHONE}px: {over:?}");
    rig.close().await;
}

#[test_id(tqybeuj)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn screenshots_of_the_page_mid_file_with_and_without_the_bar() {
    let rig = rig().await;
    let page = &rig.page;
    for (width, height, mobile) in [(PHONE, HEIGHT, true), (1280, 900, false)] {
        viewport(page, width, height, mobile).await;
        for scheme in ["light", "dark"] {
            colour_scheme(page, scheme).await;
            eval::<bool>(
                page,
                "(document.querySelector('#doc-2 [data-line=\"60\"]').scrollIntoView({block: 'center'}), true)",
            )
            .await;
            wait_for(page, "the bar showed", BAR_SHOWN).await;
            let name = |kind: &str| format!("bar-{kind}-{width}-{scheme}.png");
            // "Before" is the same page with the bar taken away.
            eval::<bool>(
                page,
                "(document.querySelector('.bar').style.display = 'none', true)",
            )
            .await;
            screenshot(page, &name("before")).await;
            eval::<bool>(
                page,
                "(document.querySelector('.bar').style.display = '', true)",
            )
            .await;
            screenshot(page, &name("after")).await;
        }
    }
    rig.close().await;
}
