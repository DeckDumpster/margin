//! Touch gestures and the margin, driven with CDP `Input.dispatchTouchEvent` at phone size over
//! the 3,000-line fixture. Run by `gate.sh` with `-- --ignored`; refuses by name when
//! `MARGIN_CHROMIUM` is unset. Waits are on signals from the page, never on elapsed time.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams, SetTouchEmulationEnabledParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchTouchEventParams, DispatchTouchEventReturns, DispatchTouchEventType, TimeSinceEpoch,
    TouchPoint,
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
        self.browser.wait().await.unwrap();
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

/// `touchEnd` and `touchCancel` must carry `"touchPoints": []`: CDP makes the key mandatory, and
/// `DispatchTouchEventParams` omits it when empty.
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

/// `stamp` is the event's CDP timestamp in seconds; `None` lets Chrome stamp it on arrival.
async fn touch(
    page: &Page,
    kind: DispatchTouchEventType,
    at: Option<(f64, f64)>,
    stamp: Option<f64>,
) {
    let timestamp = stamp.map(TimeSinceEpoch::new);
    let points = at
        .map(|(x, y)| vec![TouchPoint::new(x, y)])
        .unwrap_or_default();
    if matches!(
        kind,
        DispatchTouchEventType::TouchEnd | DispatchTouchEventType::TouchCancel
    ) {
        let params = EndTouch {
            kind: kind.clone(),
            touch_points: points,
            timestamp,
        };
        let json = serde_json::to_string(&params).unwrap();
        page.execute(params)
            .await
            .unwrap_or_else(|e| panic!("dispatching {kind:?} {json}: {e}"));
        return;
    }
    let mut params = DispatchTouchEventParams::new(kind.clone(), points);
    params.timestamp = timestamp;
    let json = serde_json::to_string(&params).unwrap();
    page.execute(params)
        .await
        .unwrap_or_else(|e| panic!("dispatching {kind:?} {json}: {e}"));
}

/// Scroll line `n` to the middle of the viewport and return the centre of its number.
async fn number_of(page: &Page, n: u32) -> (f64, f64) {
    let (x, y): (f64, f64) = eval(
        page,
        &format!(
            "(() => {{ const row = document.querySelector('[data-line=\"{n}\"]');
              row.scrollIntoView({{block: 'center'}});
              const r = row.querySelector('.no').getBoundingClientRect();
              return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
        ),
    )
    .await;
    (x, y)
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

#[test_id(tyqz1nk)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_tap_on_line_10_opens_the_box_under_line_10() {
    let rig = rig().await;
    let (x, y) = number_of(&rig.page, 10).await;
    touch(
        &rig.page,
        DispatchTouchEventType::TouchStart,
        Some((x, y)),
        None,
    )
    .await;
    touch(&rig.page, DispatchTouchEventType::TouchEnd, None, None).await;
    wait_for(
        &rig.page,
        "the box opened",
        "document.querySelector('.newbox')",
    )
    .await;
    let under: String = eval(
        &rig.page,
        "document.querySelector('.newbox').previousElementSibling.getAttribute('data-line')",
    )
    .await;
    assert_eq!(under, "10");
    rig.close().await;
}

#[test_id(tdgv5ru)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_held_press_dragged_to_the_bottom_edge_scrolls_and_posts_a_range_from_100() {
    let rig = rig().await;
    let page = &rig.page;
    let (x, y) = number_of(page, 100).await;
    let seen_before: u32 = eval(
        page,
        "Math.max(...[...document.querySelectorAll('[data-line]')]
           .filter((r) => r.getBoundingClientRect().bottom <= innerHeight)
           .map((r) => Number(r.dataset.line)))",
    )
    .await;
    let top_before: f64 = eval(page, "window.scrollY").await;

    touch(page, DispatchTouchEventType::TouchStart, Some((x, y)), None).await;
    wait_for(
        page,
        "the hold began a range",
        "document.body.dataset.gesture === 'ranging'",
    )
    .await;
    // Drag to the bottom edge and hold there: the page scrolls on its own.
    let edge = f64::from(HEIGHT) - 12.0;
    touch(
        page,
        DispatchTouchEventType::TouchMove,
        Some((x, (y + edge) / 2.0)),
        None,
    )
    .await;
    touch(
        page,
        DispatchTouchEventType::TouchMove,
        Some((x, edge)),
        None,
    )
    .await;
    wait_for(
        page,
        "the page scrolled by itself",
        &format!("window.scrollY > {}", top_before + 200.0),
    )
    .await;
    wait_for(
        page,
        "the range passed the first screen",
        &format!(
            "[...document.querySelectorAll('.ln.sel')].some((r) => Number(r.dataset.line) > {})",
            seen_before + 15
        ),
    )
    .await;
    touch(page, DispatchTouchEventType::TouchEnd, None, None).await;
    wait_for(page, "the box opened", "document.querySelector('.newbox')").await;
    let (first, last): (u32, u32) = eval(
        page,
        "[Number(document.querySelector('.newbox').dataset.first), Number(document.querySelector('.newbox').dataset.last)]",
    )
    .await;
    assert_eq!(first, 100);
    assert!(
        last > seen_before + 15,
        "range ends at {last}, first screen ended at {seen_before}"
    );

    post_from_box(page, "a range over several screens").await;
    wait_for(
        page,
        "the thread appeared",
        "document.querySelector('.thread')",
    )
    .await;
    let (tf, tl, under): (u32, u32, String) = eval(
        page,
        "(() => { const t = document.querySelector('.thread'); return [Number(t.dataset.first), Number(t.dataset.last),
           t.previousElementSibling.getAttribute('data-line')]; })()",
    )
    .await;
    assert_eq!((tf, tl), (first, last), "the posted thread's anchor");
    assert_eq!(
        under,
        last.to_string(),
        "the thread opens under the last line"
    );
    let marked: u32 = eval(page, "document.querySelectorAll('.mark').length").await;
    assert_eq!(marked, last - first + 1, "one mark on each covered line");
    rig.close().await;
}

/// Swipe up from line 50 with every event stamped 16 ms after the last, so the page judges the
/// swipe by event time whatever the speed of the test. With `after_hold` the swipe starts only
/// once the hold timer has fired, as when the page handles the first move late.
async fn swipe_scrolls_and_creates_nothing(after_hold: bool) {
    let rig = rig().await;
    let page = &rig.page;
    let (x, y) = number_of(page, 50).await;
    let before: f64 = eval(page, "window.scrollY").await;
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let stamp = |n: u32| Some(start + 0.016 * f64::from(n));
    touch(
        page,
        DispatchTouchEventType::TouchStart,
        Some((x, y)),
        stamp(0),
    )
    .await;
    if after_hold {
        wait_for(
            page,
            "the hold began a range",
            "document.body.dataset.gesture === 'ranging'",
        )
        .await;
    }
    for step in 1..=6 {
        touch(
            page,
            DispatchTouchEventType::TouchMove,
            Some((x, y - 60.0 * f64::from(step))),
            stamp(step),
        )
        .await;
    }
    touch(page, DispatchTouchEventType::TouchEnd, None, stamp(7)).await;
    wait_for(
        page,
        "the page scrolled",
        &format!("window.scrollY > {}", before + 100.0),
    )
    .await;
    let state: (bool, bool, bool) = eval(
        page,
        "[!!document.querySelector('.newbox'), !!document.querySelector('.thread'), 'gesture' in document.body.dataset]",
    )
    .await;
    assert_eq!(state, (false, false, false));
    rig.close().await;
}

#[test_id(t2he8cd)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_swipe_without_the_hold_scrolls_the_page_and_creates_nothing() {
    swipe_scrolls_and_creates_nothing(false).await;
}

#[test_id(tzz4vbg)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_swipe_handled_after_the_hold_timer_scrolls_and_creates_nothing() {
    swipe_scrolls_and_creates_nothing(true).await;
}

#[test_id(tj3wmv9)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn three_overlapping_threads_open_keep_the_phone_page_inside_390px() {
    let rig = rig().await;
    let page = &rig.page;
    // Three threads on line 17: a range, an overlapping range and the line alone.
    let made: u32 = eval(
        page,
        r#"(async () => {
          const version = Number(document.querySelector('.lines').dataset.version);
          let n = 0;
          for (const [first, last] of [[10, 20], [15, 25], [17, 17]]) {
            const res = await fetch('/c/fixture/threads', { method: 'POST',
              headers: { 'content-type': 'application/json' },
              body: JSON.stringify({ version_id: version, first_line: first, last_line: last, body: 'a comment on ' + first + '-' + last }) });
            if (res.ok) n += 1;
          }
          return n;
        })()"#,
    )
    .await;
    assert_eq!(made, 3);
    page.reload()
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();

    let lanes: Vec<u32> = eval(
        page,
        "[...document.querySelectorAll('[data-line=\"17\"] .mark')].map((m) => Number(m.style.getPropertyValue('--lane')))",
    )
    .await;
    assert_eq!(lanes, [0, 1, 2]);
    // Tapping a mark opens its thread.
    eval::<bool>(
        page,
        "(() => { document.querySelectorAll('[data-line=\"17\"] .mark').forEach((m) => m.click()); return true; })()",
    )
    .await;
    wait_for(
        page,
        "three threads open",
        "document.querySelectorAll('.thread:not([hidden])').length === 3",
    )
    .await;

    let w: Widths = eval(page, MEASURE).await;
    assert!(w.page_scroll_width <= PHONE, "page scrolls sideways: {w:?}");
    assert!(w.any_overflow.is_empty(), "elements past {PHONE}px: {w:?}");
    assert!(w.outside_frame_overflow.is_empty(), "{w:?}");
    screenshot(page, "anchors-390.png").await;

    // The same page with the nowrap choice: still nothing outside the frame past 390.
    eval::<bool>(
        page,
        "(() => { document.getElementById('wrap-toggle').click(); return true; })()",
    )
    .await;
    let off: Widths = eval(page, MEASURE).await;
    assert!(off.page_scroll_width <= PHONE, "wrap off: {off:?}");
    assert!(off.outside_frame_overflow.is_empty(), "wrap off: {off:?}");
    eval::<bool>(
        page,
        "(() => { document.getElementById('wrap-toggle').click(); return true; })()",
    )
    .await;

    viewport(page, 1280, 900, false).await;
    screenshot(page, "anchors-1280.png").await;
    rig.close().await;
}

/// Make `n` single-line threads on line 17 by `fetch`; returns how many were accepted.
async fn fetch_threads_on_17(page: &Page, n: u32) -> u32 {
    eval(
        page,
        &format!(
            r#"(async () => {{
              const version = Number(document.querySelector('.lines').dataset.version);
              let made = 0;
              for (let k = 0; k < {n}; k++) {{
                const res = await fetch('/c/fixture/threads', {{ method: 'POST',
                  headers: {{ 'content-type': 'application/json' }},
                  body: JSON.stringify({{ version_id: version, first_line: 17, last_line: 17, body: 'comment ' + k }}) }});
                if (res.ok) made += 1;
              }}
              return made;
            }})()"#
        ),
    )
    .await
}

async fn reload(page: &Page) {
    page.reload()
        .await
        .unwrap()
        .wait_for_navigation()
        .await
        .unwrap();
}

/// Boxes of the marks on line 17, the gutter's box, and the code's box.
#[derive(Debug, serde::Deserialize)]
struct Boxes {
    marks: Vec<[f64; 4]>,
    gut: [f64; 4],
    code_left: f64,
    gut_width: f64,
}

async fn boxes_on_17(page: &Page) -> Boxes {
    eval(
        page,
        r#"(() => {
          const row = document.querySelector('[data-line="17"]');
          const b = (el) => { const r = el.getBoundingClientRect(); return [r.left, r.top, r.right, r.bottom]; };
          const g = row.querySelector('.gut').getBoundingClientRect();
          return { marks: [...row.querySelectorAll('.mark')].map(b), gut: b(row.querySelector('.gut')),
                   code_left: row.querySelector('.code').getBoundingClientRect().left, gut_width: g.width };
        })()"#,
    )
    .await
}

fn assert_marks_disjoint_in_gutter(b: &Boxes, count: usize, what: &str) {
    assert_eq!(b.marks.len(), count, "{what}: {b:?}");
    for (i, m) in b.marks.iter().enumerate() {
        assert!(m[2] - m[0] > 0.0, "{what}: mark {i} has no width: {b:?}");
        assert!(
            m[0] >= b.gut[0] - 0.01 && m[2] <= b.gut[2] + 0.01,
            "{what}: mark {i} outside the gutter: {b:?}"
        );
        for (j, o) in b.marks.iter().enumerate().skip(i + 1) {
            assert!(
                m[2] <= o[0] + 0.01 || o[2] <= m[0] + 0.01,
                "{what}: marks {i} and {j} overlap: {b:?}"
            );
        }
    }
    assert!(
        b.gut[2] <= b.code_left + 0.01,
        "{what}: gutter right of code: {b:?}"
    );
}

#[test_id(t23vk4f)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn every_thread_on_a_line_has_its_own_mark_that_opens_it_however_many_cover_the_line() {
    let rig = rig().await;
    let page = &rig.page;
    assert_eq!(fetch_threads_on_17(page, 4).await, 4);
    reload(page).await;

    // 1. The fifth arrives by a tap on number 17 and the box's Post.
    let (x, y) = number_of(page, 17).await;
    touch(page, DispatchTouchEventType::TouchStart, Some((x, y)), None).await;
    touch(page, DispatchTouchEventType::TouchEnd, None, None).await;
    wait_for(
        page,
        "the new-comment box",
        "document.querySelector('.newbox')",
    )
    .await;
    post_from_box(page, "the fifth").await;
    wait_for(
        page,
        "five marks on line 17",
        "document.querySelectorAll('[data-line=\"17\"] .mark').length === 5",
    )
    .await;
    number_of(page, 17).await;
    let live = boxes_on_17(page).await;
    assert_marks_disjoint_in_gutter(&live, 5, "after the post");
    for _ in 0..2 {
        reload(page).await;
        number_of(page, 17).await;
        let b = boxes_on_17(page).await;
        assert_marks_disjoint_in_gutter(&b, 5, "after a reload");
    }
    screenshot(page, "lanes-390.png").await;

    // 2. A tap at the centre of each mark opens its own thread and no other.
    let ids: Vec<String> = eval(
        page,
        "[...document.querySelectorAll('[data-line=\"17\"] .mark')].map((m) => m.dataset.thread)",
    )
    .await;
    assert_eq!(ids.len(), 5);
    eval::<bool>(
        page,
        "(() => { document.querySelectorAll('.thread').forEach((t) => { t.hidden = true; }); return true; })()",
    )
    .await;
    for (k, id) in ids.iter().enumerate() {
        let (cx, cy): (f64, f64) = eval(
            page,
            &format!(
                "(() => {{ const m = document.querySelectorAll('[data-line=\"17\"] .mark')[{k}];
                  m.scrollIntoView({{block: 'center'}});
                  const r = m.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
            ),
        )
        .await;
        touch(
            page,
            DispatchTouchEventType::TouchStart,
            Some((cx, cy)),
            None,
        )
        .await;
        touch(page, DispatchTouchEventType::TouchEnd, None, None).await;
        wait_for(
            page,
            "the tapped thread opens",
            &format!("!document.getElementById('thread-{id}').hidden"),
        )
        .await;
        let open: Vec<String> = eval(
            page,
            "[...document.querySelectorAll('.thread:not([hidden])')].map((t) => t.id)",
        )
        .await;
        assert_eq!(open, [format!("thread-{id}")], "tap on mark {k}");
        eval::<bool>(
            page,
            &format!(
                "(() => {{ document.getElementById('thread-{id}').hidden = true; return true; }})()"
            ),
        )
        .await;
    }

    // 3. Deleting one on the open page leaves four marks and a 22px gutter.
    let code18_before = code18_left(page).await;
    let last = ids.last().unwrap();
    eval::<bool>(
        page,
        &format!(
            "(() => {{ const t = document.getElementById('thread-{last}'); t.hidden = false;
              t.querySelector('button[data-act=delete]').click(); return true; }})()"
        ),
    )
    .await;
    wait_for(
        page,
        "four marks on line 17",
        "document.querySelectorAll('[data-line=\"17\"] .mark').length === 4",
    )
    .await;
    number_of(page, 17).await;
    let four = boxes_on_17(page).await;
    assert!(
        (four.gut_width - 22.0).abs() < 0.01,
        "gutter after delete: {four:?}"
    );
    assert_marks_disjoint_in_gutter(&four, 4, "after the delete");
    let gut18: f64 = eval(
        page,
        "document.querySelector('[data-line=\"18\"] .gut').getBoundingClientRect().width",
    )
    .await;
    assert!((gut18 - 22.0).abs() < 0.01, "line 18 gutter {gut18}");
    assert_eq!(code18_left(page).await, code18_before, "line 18 moved");
    rig.close().await;
}

/// Line 18's code left edge, relative to the frame's own content (scroll-independent).
async fn code18_left(page: &Page) -> f64 {
    eval(
        page,
        "document.querySelector('[data-line=\"18\"] .code').getBoundingClientRect().left",
    )
    .await
}

#[test_id(trrdqnz)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn twelve_threads_on_a_line_keep_disjoint_marks_and_the_phone_page_inside_390px() {
    let rig = rig().await;
    let page = &rig.page;
    assert_eq!(fetch_threads_on_17(page, 12).await, 12);
    reload(page).await;
    number_of(page, 17).await;
    let b = boxes_on_17(page).await;
    assert_marks_disjoint_in_gutter(&b, 12, "twelve threads");
    let w: Widths = eval(page, MEASURE).await;
    assert!(w.page_scroll_width <= PHONE, "page scrolls sideways: {w:?}");
    assert!(w.outside_frame_overflow.is_empty(), "{w:?}");
    assert!(w.any_overflow.is_empty(), "elements past {PHONE}px: {w:?}");
    eval::<bool>(
        page,
        "(() => { document.getElementById('wrap-toggle').click(); return true; })()",
    )
    .await;
    let off: Widths = eval(page, MEASURE).await;
    assert!(off.page_scroll_width <= PHONE, "wrap off: {off:?}");
    assert!(off.outside_frame_overflow.is_empty(), "wrap off: {off:?}");
    viewport(page, 1280, 900, false).await;
    screenshot(page, "lanes-1280.png").await;
    rig.close().await;
}

async fn tap_number(page: &Page, n: u32) {
    let (x, y) = number_of(page, n).await;
    touch(page, DispatchTouchEventType::TouchStart, Some((x, y)), None).await;
    touch(page, DispatchTouchEventType::TouchEnd, None, None).await;
}

async fn set_box_text(page: &Page, first: u32, text: &str) {
    eval::<bool>(
        page,
        &format!(
            "(() => {{ document.querySelector('.newbox[data-first=\"{first}\"] textarea').value = {text:?}; return true; }})()"
        ),
    )
    .await;
}

async fn box_text(page: &Page, first: u32) -> String {
    eval(
        page,
        &format!("document.querySelector('.newbox[data-first=\"{first}\"] textarea').value"),
    )
    .await
}

#[test_id(t0dlf8g)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_box_holding_text_stays_when_another_line_is_tapped() {
    let rig = rig().await;
    let page = &rig.page;
    tap_number(page, 10).await;
    wait_for(
        page,
        "line 10 box",
        "document.querySelector('.newbox[data-first=\"10\"]')",
    )
    .await;
    set_box_text(page, 10, "off").await;
    tap_number(page, 12).await;
    wait_for(
        page,
        "line 12 box",
        "document.querySelector('.newbox[data-first=\"12\"]')",
    )
    .await;
    let under: String = eval(
        page,
        "document.querySelector('.newbox[data-first=\"10\"]').previousElementSibling.getAttribute('data-line')",
    )
    .await;
    assert_eq!(under, "10");
    assert_eq!(box_text(page, 10).await, "off");
    assert_eq!(box_text(page, 12).await, "");

    // Tapping line 10 again focuses the same box rather than opening a second.
    tap_number(page, 10).await;
    wait_for(
        page,
        "the line-10 box focused",
        "document.activeElement === document.querySelector('.newbox[data-first=\"10\"] textarea')",
    )
    .await;
    let n: u32 = eval(
        page,
        "document.querySelectorAll('.newbox[data-first=\"10\"]').length",
    )
    .await;
    assert_eq!(n, 1);
    assert_eq!(box_text(page, 10).await, "off");

    // Cancel removes only its own box. The empty line-12 box closed above; open it again.
    tap_number(page, 12).await;
    wait_for(
        page,
        "line 12 box",
        "document.querySelector('.newbox[data-first=\"12\"]')",
    )
    .await;
    set_box_text(page, 12, "twelve").await;
    tap_number(page, 14).await;
    wait_for(
        page,
        "line 14 box",
        "document.querySelector('.newbox[data-first=\"14\"]')",
    )
    .await;
    set_box_text(page, 14, "fourteen").await;
    eval::<bool>(
        page,
        "(() => { document.querySelector('.newbox[data-first=\"14\"] button[data-act=cancel]').click(); return true; })()",
    )
    .await;
    let left: Vec<String> = eval(
        page,
        "[...document.querySelectorAll('.newbox')].map((f) => f.dataset.first)",
    )
    .await;
    assert_eq!(left, ["10", "12"]);

    // Posting the line-10 box leaves the line-12 text alone.
    eval::<bool>(
        page,
        "(() => { document.querySelector('.newbox[data-first=\"10\"] button[type=submit]').click(); return true; })()",
    )
    .await;
    wait_for(
        page,
        "the thread on line 10",
        "document.querySelector('.thread[data-first=\"10\"]')",
    )
    .await;
    assert_eq!(box_text(page, 12).await, "twelve");
    rig.close().await;
}

#[test_id(tvfgojj)]
#[tokio::test]
#[ignore = "needs MARGIN_CHROMIUM; gate.sh runs it with --ignored"]
async fn a_held_range_keeps_the_text_of_an_open_box_and_empty_boxes_close() {
    let rig = rig().await;
    let page = &rig.page;
    tap_number(page, 10).await;
    wait_for(
        page,
        "line 10 box",
        "document.querySelector('.newbox[data-first=\"10\"]')",
    )
    .await;
    set_box_text(page, 10, "keep me").await;

    let (x, y) = number_of(page, 20).await;
    touch(page, DispatchTouchEventType::TouchStart, Some((x, y)), None).await;
    wait_for(
        page,
        "ranging",
        "document.body.dataset.gesture === 'ranging'",
    )
    .await;
    let (_, y22) = number_of_without_scroll(page, 22).await;
    touch(
        page,
        DispatchTouchEventType::TouchMove,
        Some((x, y22)),
        None,
    )
    .await;
    wait_for(
        page,
        "line 22 selected",
        "document.querySelector('[data-line=\"22\"].sel')",
    )
    .await;
    touch(page, DispatchTouchEventType::TouchEnd, None, None).await;
    wait_for(
        page,
        "range box",
        "document.querySelector('.newbox[data-first=\"20\"][data-last=\"22\"]')",
    )
    .await;
    assert_eq!(box_text(page, 10).await, "keep me");

    // Empty boxes close when another opens.
    eval::<bool>(
        page,
        "(() => { document.querySelectorAll('.newbox').forEach((f) => f.remove()); return true; })()",
    )
    .await;
    tap_number(page, 10).await;
    wait_for(
        page,
        "line 10 box",
        "document.querySelector('.newbox[data-first=\"10\"]')",
    )
    .await;
    tap_number(page, 12).await;
    wait_for(
        page,
        "line 12 box",
        "document.querySelector('.newbox[data-first=\"12\"]')",
    )
    .await;
    let left: Vec<String> = eval(
        page,
        "[...document.querySelectorAll('.newbox')].map((f) => f.dataset.first)",
    )
    .await;
    assert_eq!(left, ["12"]);
    rig.close().await;
}

/// The centre of line `n`'s number where it is now, without scrolling.
async fn number_of_without_scroll(page: &Page, n: u32) -> (f64, f64) {
    eval(
        page,
        &format!(
            "(() => {{ const r = document.querySelector('[data-line=\"{n}\"] .no').getBoundingClientRect();
              return [r.left + r.width / 2, r.top + r.height / 2]; }})()"
        ),
    )
    .await
}
