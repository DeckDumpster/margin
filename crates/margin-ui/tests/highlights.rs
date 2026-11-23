//! Highlights of words, driven at phone size over the 3,000-line fixture: the selection is set
//! programmatically (a native hold-and-drag needs the phone's handles). Run by `gate.sh` with `-- --ignored`; refuses by name when
//! `MARGIN_CHROMIUM` is unset. Waits are on signals from the page, never on elapsed time.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams, SetTouchEmulationEnabledParams,
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
const HEIGHT: u32 = 844;
const SIGNAL_DEADLINE: Duration = Duration::from_secs(20);

struct Rig {
    browser: Browser,
    page: Page,
    pump: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
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

async fn rig() -> Rig {
    let (url, dir) = serve().await;
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
        _dir: dir,
    }
}

impl Rig {
    async fn close(mut self) {
        self.browser.close().await.unwrap();
        let _ = self.pump.await;
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

fn shot_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Evidence only: nothing asserts on these.
async fn screenshot(page: &Page, name: &str) {
    page.evaluate("document.querySelector('[data-line=\"12\"]').scrollIntoView({block: 'start'})")
        .await
        .unwrap();
    page.save_screenshot(ScreenshotParams::builder().build(), shot_dir().join(name))
        .await
        .unwrap();
    page.evaluate("window.scrollTo(0, 0)").await.unwrap();
}

/// Type into the open new-comment box and press Post.
async fn post_from_box(page: &Page, text: &str) {
    eval::<bool>(
        page,
        &format!(
            "(() => {{ const f = document.querySelector('.newbox');
              f.querySelector('textarea').value = {text:?};
              f.querySelector('button[type=submit]').click(); return true; }})()"
        ),
    )
    .await;
}

#[derive(Debug, serde::Deserialize)]
struct Widths {
    page_scroll_width: u32,
    outside_frame_overflow: Vec<String>,
    any_overflow: Vec<String>,
}

const MEASURE: &str = r#"(() => {
  const W = 390;
  const over = (el) => el.getBoundingClientRect().right > W + 0.5;
  const label = (el) => el.tagName.toLowerCase() + '.' + el.className + ' right=' + el.getBoundingClientRect().right;
  const all = [...document.body.querySelectorAll('*')];
  return {
    page_scroll_width: document.documentElement.scrollWidth,
    outside_frame_overflow: all.filter((el) => !el.closest('.frame') && over(el)).map(label),
    any_overflow: all.filter(over).slice(0, 5).map(label),
  };
})()"#;

/// Scroll line `first` to the middle and select columns `c1` of `first` through `c2` of `last`.
async fn select(page: &Page, first: u32, c1: u32, last: u32, c2: u32) {
    eval::<bool>(
        page,
        &format!(
            "(() => {{
              const at = (n, col) => {{
                const code = document.querySelector('[data-line=\"' + n + '\"] .code');
                const walker = document.createTreeWalker(code, NodeFilter.SHOW_TEXT);
                let left = col;
                for (let t = walker.nextNode(); t; t = walker.nextNode()) {{
                  if (left <= t.data.length) return [t, left];
                  left -= t.data.length;
                }}
                throw new Error('column past the line');
              }};
              document.querySelector('[data-line=\"{first}\"]').scrollIntoView({{block: 'center'}});
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
        ),
    )
    .await;
}

/// Press the Comment button, type and post; wait until `threads` threads are on the page.
async fn comment_on_selection(page: &Page, text: &str, threads: u32) {
    wait_for(
        page,
        "the Comment button showed",
        "(() => { const b = document.getElementById('hl-comment'); return b && !b.hidden; })()",
    )
    .await;
    eval::<bool>(
        page,
        "(() => { document.getElementById('hl-comment').click(); return true; })()",
    )
    .await;
    wait_for(
        page,
        "the box opened with columns",
        "document.querySelector('.newbox[data-start-col]')",
    )
    .await;
    post_from_box(page, text).await;
    wait_for(
        page,
        "the thread appeared",
        &format!("document.querySelectorAll('.thread').length === {threads}"),
    )
    .await;
}

fn fixture_line(n: usize) -> &'static str {
    FIXTURE.lines().nth(n - 1).unwrap()
}

#[test_id(tvie224)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn selected_words_across_two_lines_get_a_button_and_become_a_tinted_thread() {
    let rig = rig().await;
    let page = &rig.page;
    select(page, 10, 3, 11, 10).await;
    wait_for(
        page,
        "the Comment button showed",
        "(() => { const b = document.getElementById('hl-comment'); return b && !b.hidden; })()",
    )
    .await;
    let (l, t, r, b): (f64, f64, f64, f64) = eval(
        page,
        "(() => { const x = document.getElementById('hl-comment').getBoundingClientRect(); return [x.left, x.top, x.right, x.bottom]; })()",
    )
    .await;
    assert!(
        l >= 0.0 && t >= 0.0 && r <= f64::from(PHONE) && b <= f64::from(HEIGHT),
        "button outside the viewport: {l} {t} {r} {b}"
    );
    let sel_bottom: f64 = eval(
        page,
        "(() => { const rs = getSelection().getRangeAt(0).getClientRects(); return rs[rs.length - 1].bottom; })()",
    )
    .await;
    assert!(
        t >= sel_bottom,
        "the button sits below the selection's end, clear of the phone's menu above it"
    );

    comment_on_selection(page, "these words", 1).await;
    let anchor: (u32, u32) = eval(
        page,
        "(() => { const t = document.querySelector('.thread'); return [Number(t.dataset.first), Number(t.dataset.last)]; })()",
    )
    .await;
    assert_eq!(anchor, (10, 11));
    let tinted: String = eval(
        page,
        "[...document.querySelectorAll('.hl')].map((h) => h.textContent).join('')",
    )
    .await;
    let want = format!(
        "{}{}",
        fixture_line(10).chars().skip(3).collect::<String>(),
        fixture_line(11).chars().take(10).collect::<String>()
    );
    assert_eq!(tinted, want, "the tinted text is the selected text");
    let kept: bool = eval(
        page,
        "!document.getElementById('hl-comment') || document.getElementById('hl-comment').hidden",
    )
    .await;
    assert!(kept, "the button goes once the box is open");

    // Reload: the server renders the same tint.
    page.reload()
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();
    let again: String = eval(
        page,
        "[...document.querySelectorAll('.hl')].map((h) => h.textContent).join('')",
    )
    .await;
    assert_eq!(again, want, "the server's tint matches the script's");

    // Tapping tinted words opens the thread.
    eval::<bool>(
        page,
        "(() => { document.querySelector('.hl').click(); return true; })()",
    )
    .await;
    wait_for(
        page,
        "the thread opened",
        "document.querySelector('.thread:not([hidden])')",
    )
    .await;
    rig.close().await;
}

#[test_id(tax80my)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_second_overlapping_highlight_leaves_the_shared_words_with_both_lanes() {
    let rig = rig().await;
    let page = &rig.page;
    select(page, 10, 3, 11, 10).await;
    comment_on_selection(page, "first", 1).await;
    // "publish, distribute," on line 11 shares "publish" (columns 3..10) with the first.
    select(page, 11, 3, 11, 23).await;
    comment_on_selection(page, "second", 2).await;

    let shared: Vec<String> = eval(
        page,
        "[...document.querySelectorAll('[data-line=\"11\"] .hl.hl-l0.hl-l1')].map((h) => h.textContent)",
    )
    .await;
    let want: String = fixture_line(11).chars().skip(3).take(7).collect();
    assert_eq!(
        shared.concat(),
        want,
        "the shared words carry both lane classes"
    );
    let lanes: Vec<u32> = eval(
        page,
        "[...document.querySelectorAll('[data-line=\"11\"] .mark')].map((m) => Number(m.dataset.lane))",
    )
    .await;
    assert_eq!(lanes, [0, 1], "each highlight's margin mark has its lane");
    let colours: (String, String) = eval(
        page,
        "(() => { const c = (l) => getComputedStyle(document.querySelector('[data-line=\"11\"] .mark[data-lane=\"' + l + '\"]')).backgroundColor; return [c(0), c(1)]; })()",
    )
    .await;
    assert_ne!(colours.0, colours.1, "lanes have distinct colours");

    // Tapping the shared words opens both threads (posting left them open, so close them first).
    eval::<bool>(
        page,
        "(() => { document.querySelectorAll('.thread').forEach((t) => { t.hidden = true; }); document.querySelector('[data-line=\"11\"] .hl.hl-l0.hl-l1').click(); return true; })()",
    )
    .await;
    wait_for(
        page,
        "both threads open",
        "document.querySelectorAll('.thread:not([hidden])').length === 2",
    )
    .await;

    let w: Widths = eval(page, MEASURE).await;
    assert!(w.page_scroll_width <= PHONE, "page scrolls sideways: {w:?}");
    assert!(w.any_overflow.is_empty(), "elements past {PHONE}px: {w:?}");
    assert!(w.outside_frame_overflow.is_empty(), "{w:?}");
    screenshot(page, "highlights-390.png").await;
    viewport(page, 1280, 900, false).await;
    screenshot(page, "highlights-1280.png").await;
    rig.close().await;
}

#[test_id(t355qrw)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn commenting_on_words_keeps_the_text_of_other_open_boxes() {
    let rig = rig().await;
    let page = &rig.page;
    // Open a line-10 box by the same path a tap takes: the number's pointer events.
    eval::<bool>(
        page,
        "(() => { const n = document.querySelector('[data-line=\"10\"] .no');
          const r = n.getBoundingClientRect();
          const o = { bubbles: true, pointerType: 'touch', isPrimary: true, clientX: r.left + 2, clientY: r.top + 2 };
          n.dispatchEvent(new PointerEvent('pointerdown', o));
          n.dispatchEvent(new PointerEvent('pointerup', o)); return true; })()",
    )
    .await;
    wait_for(
        page,
        "line 10 box",
        "document.querySelector('.newbox[data-first=\"10\"]')",
    )
    .await;
    eval::<bool>(
        page,
        "(() => { document.querySelector('.newbox[data-first=\"10\"] textarea').value = 'ten'; return true; })()",
    )
    .await;

    select(page, 12, 3, 12, 10).await;
    wait_for(
        page,
        "the Comment button showed",
        "(() => { const b = document.getElementById('hl-comment'); return b && !b.hidden; })()",
    )
    .await;
    eval::<bool>(
        page,
        "(() => { document.getElementById('hl-comment').click(); return true; })()",
    )
    .await;
    wait_for(
        page,
        "words box",
        "document.querySelector('.newbox[data-start-col]')",
    )
    .await;
    let ten: String = eval(
        page,
        "document.querySelector('.newbox[data-first=\"10\"] textarea').value",
    )
    .await;
    assert_eq!(ten, "ten");

    eval::<bool>(
        page,
        "(() => { document.querySelector('.newbox[data-start-col] textarea').value = 'words'; return true; })()",
    )
    .await;
    select(page, 14, 3, 14, 10).await;
    wait_for(
        page,
        "the Comment button showed",
        "(() => { const b = document.getElementById('hl-comment'); return b && !b.hidden; })()",
    )
    .await;
    eval::<bool>(
        page,
        "(() => { document.getElementById('hl-comment').click(); return true; })()",
    )
    .await;
    wait_for(
        page,
        "second words box",
        "document.querySelector('.newbox[data-first=\"14\"][data-start-col]')",
    )
    .await;
    let texts: Vec<String> = eval(
        page,
        "[...document.querySelectorAll('.newbox')].map((f) => f.querySelector('textarea').value)",
    )
    .await;
    assert_eq!(texts, ["ten", "words", ""]);
    rig.close().await;
}
