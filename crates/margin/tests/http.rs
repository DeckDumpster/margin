//! The server in-process (no binary spawned) over a temporary database.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use margin::agents::hash_token;
use margin::api::{AppState, router};
use margin::config::{Config, ConfigError};
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin_core::{Clock, Rng};
use margin_test_id::test_id;
use serde_json::{Value, json};
use tower::ServiceExt;

const FIXTURE: &str = include_str!("../../margin-ui/fixtures/long.rs");
const TOKEN: &str = "test-token-0123456789";

struct FixedClock(i64);
impl Clock for FixedClock {
    fn now_unix(&self) -> i64 {
        self.0
    }
}

/// Deterministic randomness: a counter spread over the bytes asked for.
struct CountingRng(AtomicU64);
impl Rng for CountingRng {
    fn fill(&self, buf: &mut [u8]) {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (n >> ((i % 8) * 8)) as u8 ^ (i as u8).wrapping_mul(31);
        }
    }
}

struct Harness {
    app: Router,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents.toml");
    std::fs::write(&agents, format!("tester = \"{}\"\n", hash_token(TOKEN))).unwrap();
    let config = Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\nbase_url = \"http://margin.test:7340/\"\ndatabase = \"{}\"\nagents_file = \"{}\"\notlp_endpoint = \"http://127.0.0.1:1/v1/metrics\"\n",
        dir.path().join("m.db").display(),
        agents.display()
    ))
    .unwrap();
    let state = AppState {
        db: Db::open(&config.database).unwrap(),
        config: Arc::new(config),
        clock: Arc::new(FixedClock(1_700_000_000)),
        rng: Arc::new(CountingRng(AtomicU64::new(1))),
        metrics: Metrics::detached(),
        highlighter: Arc::new(Highlighter::new()),
    };
    Harness {
        app: router(state),
        _dir: dir,
    }
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, String) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

fn doc(path: &str, text: &str) -> Value {
    json!({"path": path, "kind": "code", "text": text})
}

async fn post(app: &Router, title: &str, docs: Vec<Value>) -> Value {
    let (status, body) = send(
        app,
        Method::POST,
        "/api/collections",
        Some(TOKEN),
        Some(json!({"title": title, "cover_note": "read me first", "documents": docs})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    serde_json::from_str(&body).unwrap()
}

/// (line number, visible text of the code cell) for each `data-line` element, in page order.
fn page_lines(html: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for chunk in html.split("data-line=\"").skip(1) {
        let n: usize = chunk[..chunk.find('"').unwrap()].parse().unwrap();
        let code = chunk.split("<span class=\"code\">").nth(1).unwrap();
        let code = code.split("</span></div>").next().unwrap();
        let mut text = String::new();
        let mut in_tag = false;
        for c in code.chars() {
            match c {
                '<' => in_tag = true,
                '>' => in_tag = false,
                c if !in_tag => text.push(c),
                _ => {}
            }
        }
        let text = text
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&");
        out.push((n, text));
    }
    out
}

#[test_id(tzuvko0)]
#[tokio::test]
async fn the_fixture_page_shows_every_line_once_in_order() {
    let h = harness();
    let created = post(&h.app, "fixture", vec![doc("long.rs", FIXTURE)]).await;
    assert_eq!(
        created["url"],
        format!(
            "http://margin.test:7340/c/{}",
            created["id"].as_str().unwrap()
        )
    );
    let (status, html) = send(
        &h.app,
        Method::GET,
        &format!("/c/{}", created["id"].as_str().unwrap()),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let lines = page_lines(&html);
    let want: Vec<&str> = margin_core::lines::split(FIXTURE);
    assert_eq!(want.len(), 3000);
    assert_eq!(lines.len(), 3000);
    for (i, ((n, text), want)) in lines.iter().zip(&want).enumerate() {
        assert_eq!(*n, i + 1);
        assert_eq!(text, want, "line {n}");
    }
    assert_eq!(html.matches("data-line=").count(), 3000);
}

#[test_id(t9nj3bj)]
#[tokio::test]
async fn a_rust_document_carries_syntect_classes_and_multiline_spans_stay_balanced() {
    let h = harness();
    let created = post(
        &h.app,
        "rs",
        vec![doc(
            "a.rs",
            "/* one\n   two */\nfn main() { let x = \"s\"; }\n",
        )],
    )
    .await;
    let (_, html) = send(
        &h.app,
        Method::GET,
        &format!("/c/{}", created["id"].as_str().unwrap()),
        None,
        None,
    )
    .await;
    assert!(html.contains("class=\"keyword"), "{html}");
    assert!(html.contains("class=\"comment"), "{html}");
    assert!(html.contains("class=\"string"), "{html}");
    let body = html.split("<div class=\"lines\"").nth(1).unwrap();
    assert_eq!(
        body.matches("<span").count(),
        body.matches("</span>").count()
    );
}

#[test_id(t0ogysl)]
#[tokio::test]
async fn prose_is_shown_as_its_markdown_source() {
    let h = harness();
    let (_, body) = send(
        &h.app,
        Method::POST,
        "/api/collections",
        Some(TOKEN),
        Some(json!({"title": "d", "cover_note": "", "documents": [{"path": "d.md", "kind": "prose", "text": "# Head <b>\n\n* item\n"}]})),
    )
    .await;
    let id = serde_json::from_str::<Value>(&body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, html) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    let lines = page_lines(&html);
    assert_eq!(
        lines,
        [
            (1, "# Head <b>".to_string()),
            (2, String::new()),
            (3, "* item".to_string())
        ]
    );
    assert!(!html.contains("<b>"), "prose is escaped, never rendered");
}

#[test_id(t3hmmb3)]
#[tokio::test]
async fn collections_support_create_read_update_delete_and_list() {
    let h = harness();
    let created = post(
        &h.app,
        "first",
        vec![doc("a.rs", "fn a() {}\n"), doc("b.rs", "fn b() {}\n")],
    )
    .await;
    let id = created["id"].as_str().unwrap();

    let (status, body) = send(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let got: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(got["title"], "first");
    assert_eq!(got["state"], "open");
    assert_eq!(got["created_by"], "tester");
    assert_eq!(got["documents"][0]["path"], "a.rs");
    assert_eq!(got["documents"][1]["path"], "b.rs");
    assert_eq!(got["documents"][1]["version"], 1);

    let (status, body) = send(
        &h.app,
        Method::PATCH,
        &format!("/api/collections/{id}"),
        Some(TOKEN),
        Some(json!({"title": "renamed", "state": "published"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let patched: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (patched["title"].as_str(), patched["state"].as_str()),
        (Some("renamed"), Some("published"))
    );
    assert_eq!(patched["cover_note"], "read me first");

    let (_, body) = send(&h.app, Method::GET, "/api/collections", None, None).await;
    let listed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), 1);
    assert_eq!(listed["items"][0]["title"], "renamed");

    let (status, _) = send(
        &h.app,
        Method::DELETE,
        &format!("/api/collections/{id}"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &h.app,
        Method::DELETE,
        &format!("/api/collections/{id}"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test_id(t5hs72h)]
#[tokio::test]
async fn the_list_paginates_by_cursor_without_repeats() {
    let h = harness();
    let mut made = Vec::new();
    for i in 0..5 {
        made.push(
            post(&h.app, &format!("c{i}"), vec![doc("a.rs", "x\n")]).await["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    let mut seen = Vec::new();
    let mut after = String::new();
    let mut pages = 0;
    loop {
        let uri = if after.is_empty() {
            "/api/collections?limit=2".to_string()
        } else {
            format!("/api/collections?limit=2&after={after}")
        };
        let (status, body) = send(&h.app, Method::GET, &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Value = serde_json::from_str(&body).unwrap();
        pages += 1;
        for item in page["items"].as_array().unwrap() {
            seen.push(item["id"].as_str().unwrap().to_string());
        }
        match page["next"].as_str() {
            Some(c) => after = c.to_string(),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    let mut want = made.clone();
    want.sort();
    assert_eq!(sorted, want);
    assert_eq!(seen.len(), 5);
}

#[test_id(tmztohy)]
#[tokio::test]
async fn writes_without_a_valid_token_get_401() {
    let h = harness();
    let created = post(&h.app, "t", vec![doc("a.rs", "x\n")]).await;
    let id = created["id"].as_str().unwrap();
    let body = json!({"title": "t", "cover_note": "", "documents": [doc("a.rs", "x")]});
    for token in [None, Some("wrong"), Some("")] {
        let (s, _) = send(
            &h.app,
            Method::POST,
            "/api/collections",
            token,
            Some(body.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "POST with {token:?}");
        let (s, _) = send(
            &h.app,
            Method::PATCH,
            &format!("/api/collections/{id}"),
            token,
            Some(json!({"title": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "PATCH with {token:?}");
        let (s, _) = send(
            &h.app,
            Method::DELETE,
            &format!("/api/collections/{id}"),
            token,
            None,
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "DELETE with {token:?}");
    }
    let (s, _) = send(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "reads need no token; the tailnet is the guard"
    );
}

#[test_id(t93ra7m)]
#[tokio::test]
async fn malformed_collections_are_refused() {
    let h = harness();
    for body in [
        json!({"title": "", "documents": [doc("a", "x")]}),
        json!({"title": "t", "documents": []}),
        json!({"title": "t", "documents": [doc("a", "x"), doc("a", "y")]}),
        json!({"title": "t", "documents": [{"path": "a", "kind": "video", "text": "x"}]}),
    ] {
        let (s, b) = send(
            &h.app,
            Method::POST,
            "/api/collections",
            Some(TOKEN),
            Some(body.clone()),
        )
        .await;
        assert!(s.is_client_error(), "{body} gave {s}: {b}");
    }
}

#[test_id(tspr333)]
#[tokio::test]
async fn healthz_reports_the_build_sha() {
    let h = harness();
    let (s, body) = send(&h.app, Method::GET, "/healthz", None, None).await;
    assert_eq!(s, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["sha"], margin::BUILD_SHA);
}

#[test_id(t93h820)]
#[test]
fn a_config_missing_a_key_is_refused_by_that_key() {
    let full = [
        ("listen", "127.0.0.1:7340"),
        ("base_url", "http://margin.test"),
        ("database", "/var/m.db"),
        ("agents_file", "/etc/agents.toml"),
        ("otlp_endpoint", "http://127.0.0.1:4318"),
    ];
    for skip in 0..full.len() {
        let text: String = full
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != skip)
            .map(|(_, (k, v))| format!("{k} = \"{v}\"\n"))
            .collect();
        let err = Config::parse(&text).unwrap_err();
        assert!(
            err.to_string().contains(&format!("`{}`", full[skip].0)),
            "{err}"
        );
    }
    let text: String = full
        .iter()
        .map(|(k, v)| format!("{k} = \"{v}\"\n"))
        .collect();
    assert!(Config::parse(&text).is_ok());
}

#[test_id(tsjstn2)]
#[test]
fn an_unspecified_listen_address_is_refused() {
    for listen in ["0.0.0.0:7340", "[::]:7340"] {
        let text = format!(
            "listen = \"{listen}\"\nbase_url = \"http://x\"\ndatabase = \"d\"\nagents_file = \"a\"\notlp_endpoint = \"o\"\n"
        );
        assert!(
            matches!(Config::parse(&text), Err(ConfigError::UnspecifiedListen(_))),
            "{listen}"
        );
    }
}

/// A harness whose database has one worker and one queue slot, plus the metrics it pushes.
fn tiny_queue_harness() -> (
    Router,
    Db,
    std::sync::mpsc::Receiver<margin::metrics::Event>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents.toml");
    std::fs::write(&agents, "").unwrap();
    let config = Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\nbase_url = \"http://margin.test\"\ndatabase = \"{}\"\nagents_file = \"{}\"\notlp_endpoint = \"http://127.0.0.1:1\"\n",
        dir.path().join("m.db").display(),
        agents.display()
    ))
    .unwrap();
    let db = Db::open_with(&config.database, 1, 1).unwrap();
    let (metrics, events) = Metrics::capture();
    let state = AppState {
        db: db.clone(),
        config: Arc::new(config),
        clock: Arc::new(FixedClock(1)),
        rng: Arc::new(CountingRng(AtomicU64::new(1))),
        metrics,
        highlighter: Arc::new(Highlighter::new()),
    };
    (router(state), db, events, dir)
}

#[test_id(tk49gd7)]
#[tokio::test]
async fn a_full_database_queue_answers_503_and_pushes_the_metric() {
    use margin::metrics::Event;
    let (app, db, events, _dir) = tiny_queue_harness();
    let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let parked = tokio::spawn({
        let db = db.clone();
        async move {
            db.call(move |_| {
                started_tx.send(()).unwrap();
                park_rx.recv().unwrap();
            })
            .await
        }
    });
    // Wait on the worker's own signal that it holds the first job.
    tokio::task::spawn_blocking(move || started_rx.recv().unwrap())
        .await
        .unwrap();
    // Fill the one slot: the first poll of `call` enqueues the job.
    let mut filler = Box::pin(db.call(|_| ()));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(filler.as_mut(), &mut cx).is_pending());

    let (status, _) = send(&app, Method::GET, "/api/collections/anything", None, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let pushed: Vec<Event> = events.try_iter().collect();
    assert!(
        pushed.iter().any(|e| matches!(e, Event::QueueFull)),
        "{pushed:?}"
    );

    park_tx.send(()).unwrap();
    parked.await.unwrap().unwrap();
    filler.await.unwrap();
}

#[test_id(tjvet43)]
#[tokio::test]
async fn requests_and_page_renders_push_their_metrics() {
    use margin::metrics::Event;
    let (app, _db, events, _dir) = tiny_queue_harness();
    let (status, _) = send(&app, Method::GET, "/c/missing", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let pushed: Vec<Event> = events.try_iter().collect();
    assert!(
        pushed.iter().any(|e| matches!(e, Event::Request { route, status, .. } if route == "/c/{id}" && *status == 404)),
        "{pushed:?}"
    );
}

/// (collection id, version id of its one document) for a small document of `lines` lines.
async fn small_collection(h: &Harness, lines: usize) -> (String, i64) {
    let text: String = (1..=lines).map(|n| format!("line {n}\n")).collect();
    let created = post(&h.app, "t", vec![doc("a.txt", &text)]).await;
    let id = created["id"].as_str().unwrap().to_string();
    let (_, body) = send(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    let got: Value = serde_json::from_str(&body).unwrap();
    (id, got["documents"][0]["version_id"].as_i64().unwrap())
}

/// POST a thread; returns (status, fragment html, the marks header).
async fn new_thread(
    h: &Harness,
    id: &str,
    version: i64,
    first: u32,
    last: u32,
    body: &str,
) -> (StatusCode, String, Option<Value>) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/c/{id}/threads"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"version_id": version, "first_line": first, "last_line": last, "body": body})
                .to_string(),
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let marks = resp
        .headers()
        .get("x-margin-marks")
        .map(|v| serde_json::from_str(v.to_str().unwrap()).unwrap());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap(), marks)
}

fn attr(html: &str, name: &str) -> String {
    let key = format!("{name}=\"");
    let rest = html
        .split(&key)
        .nth(1)
        .unwrap_or_else(|| panic!("no {name} in {html}"));
    rest[..rest.find('"').unwrap()].to_string()
}

/// The lanes of the marks on line `n` of the reader page, left to right.
fn lanes_on_line(html: &str, n: usize) -> Vec<u32> {
    let row = html.split(&format!("data-line=\"{n}\"")).nth(1).unwrap();
    let gut = row.split("<span class=\"gut\">").nth(1).unwrap();
    let gut = gut.split("<span class=\"code\">").next().unwrap();
    gut.split("--lane:")
        .skip(1)
        .map(|c| c[..c.find('"').unwrap()].parse().unwrap())
        .collect()
}

#[test_id(tmmq8fx)]
#[tokio::test]
async fn a_line_a_range_and_an_overlapping_range_show_three_marks_in_three_lanes_on_one_line() {
    let h = harness();
    let (id, v) = small_collection(&h, 30).await;
    let (s1, f1, _) = new_thread(&h, &id, v, 10, 20, "a range").await;
    let (s2, _, _) = new_thread(&h, &id, v, 15, 25, "an overlapping range").await;
    let (s3, f3, marks) = new_thread(&h, &id, v, 17, 17, "one line inside both").await;
    assert_eq!(
        (s1, s2, s3),
        (
            StatusCode::CREATED,
            StatusCode::CREATED,
            StatusCode::CREATED
        )
    );
    assert_eq!(
        (attr(&f1, "data-first"), attr(&f1, "data-last")),
        ("10".into(), "20".into())
    );
    let marks = marks.expect("a new thread says where every mark now is");
    assert_eq!(marks.as_array().unwrap().len(), 3);

    let (_, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    assert_eq!(lanes_on_line(&page, 17), [0, 1, 2]);
    assert_eq!(lanes_on_line(&page, 12), [0]);
    assert_eq!(lanes_on_line(&page, 22), [1]);
    assert_eq!(lanes_on_line(&page, 27), Vec::<u32>::new());
    // The thread sits under the last line of its anchor, and the single line's under line 17.
    let tid = attr(&f3, "data-thread");
    let at = page.find(&format!("id=\"thread-{tid}\"")).unwrap();
    assert!(
        page.find("data-line=\"17\"").unwrap() < at,
        "thread follows line 17"
    );
    assert!(
        at < page.find("data-line=\"18\"").unwrap(),
        "thread precedes line 18"
    );
    assert!(page.contains("one line inside both"));
}

#[test_id(tf70epi)]
#[tokio::test]
async fn a_reply_appears_in_the_thread_fragment() {
    let h = harness();
    let (id, v) = small_collection(&h, 5).await;
    let (_, f, _) = new_thread(&h, &id, v, 2, 3, "first").await;
    let tid = attr(&f, "data-thread");
    let (status, body) = send(
        &h.app,
        Method::POST,
        &format!("/c/{id}/threads/{tid}/comments"),
        None,
        Some(json!({"body": "second <b>"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(
        body.contains("first") && body.contains("second &#60;b&#62;"),
        "{body}"
    );
    assert_eq!(body.matches("data-comment=").count(), 2);
    let (status, _) = send(
        &h.app,
        Method::POST,
        &format!("/c/{id}/threads/nope/comments"),
        None,
        Some(json!({"body": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test_id(tp5nwq2)]
#[tokio::test]
async fn bad_anchors_and_empty_bodies_are_refused() {
    let h = harness();
    let (id, v) = small_collection(&h, 5).await;
    for (first, last) in [(3, 2), (0, 1), (1, 6)] {
        let (status, _, _) = new_thread(&h, &id, v, first, last, "x").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{first}-{last}");
    }
    let (status, _, _) = new_thread(&h, &id, v, 1, 1, "   ").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _) = new_thread(&h, &id, v + 99, 1, 1, "x").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Put a comment by `author` into a thread straight in the store, as an agent's would be.
fn seed_comment(h: &Harness, thread: &str, author: &str) -> String {
    let conn = rusqlite::Connection::open(h._dir.path().join("m.db")).unwrap();
    conn.execute(
        "INSERT INTO comments (id, thread_id, author, body, created_at, updated_at)
         VALUES ('agentc', ?1, ?2, 'from an agent', 1, 1)",
        rusqlite::params![thread, author],
    )
    .unwrap();
    "agentc".to_string()
}

#[test_id(tlzq4a1)]
#[tokio::test]
async fn the_reader_edits_and_deletes_own_comments_but_not_an_agents() {
    let h = harness();
    let (id, v) = small_collection(&h, 5).await;
    let (_, f, _) = new_thread(&h, &id, v, 1, 1, "mine").await;
    let tid = attr(&f, "data-thread");
    let mine = attr(&f, "data-comment");
    let agent = seed_comment(&h, &tid, "tester");

    let (status, body) = send(
        &h.app,
        Method::PATCH,
        &format!("/c/{id}/comments/{mine}"),
        None,
        Some(json!({"body": "edited"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("edited") && !body.contains(">mine<"));

    let (status, _) = send(
        &h.app,
        Method::PATCH,
        &format!("/c/{id}/comments/{agent}"),
        None,
        Some(json!({"body": "hijack"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(
        &h.app,
        Method::DELETE,
        &format!("/c/{id}/comments/{agent}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Deleting the reader's comment leaves the agent's thread; the page still has it.
    let (status, body) = send(
        &h.app,
        Method::DELETE,
        &format!("/c/{id}/comments/{mine}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("from an agent") && !body.contains("edited"));
    let (status, _) = send(
        &h.app,
        Method::PATCH,
        &format!("/c/{id}/comments/zzz"),
        None,
        Some(json!({"body": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A thread with no comments left goes, and the marks answer says so.
    let (_, f2, _) = new_thread(&h, &id, v, 3, 4, "only").await;
    let only = attr(&f2, "data-comment");
    let req = Request::builder()
        .method(Method::DELETE)
        .uri(format!("/c/{id}/comments/{only}"))
        .body(Body::empty())
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let marks: Value =
        serde_json::from_str(resp.headers()["x-margin-marks"].to_str().unwrap()).unwrap();
    assert_eq!(marks.as_array().unwrap().len(), 1);
    let (_, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    assert!(lanes_on_line(&page, 3).is_empty());

    // Deleting the whole collection takes its threads and anchors with it.
    let (status, _) = send(
        &h.app,
        Method::DELETE,
        &format!("/api/collections/{id}"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[test_id(t9cgpwe)]
#[tokio::test]
async fn the_thread_list_paginates_without_repeats() {
    let h = harness();
    let (id, v) = small_collection(&h, 10).await;
    let mut made = Vec::new();
    for n in 1..=5 {
        let (_, f, _) = new_thread(&h, &id, v, n, n, &format!("c{n}")).await;
        made.push(attr(&f, "data-thread"));
    }
    let mut seen = Vec::new();
    let mut after = String::new();
    let mut pages = 0;
    loop {
        let uri = if after.is_empty() {
            format!("/c/{id}/threads?limit=2")
        } else {
            format!("/c/{id}/threads?limit=2&after={after}")
        };
        let (status, body) = send(&h.app, Method::GET, &uri, None, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let page: Value = serde_json::from_str(&body).unwrap();
        for t in page["items"].as_array().unwrap() {
            assert_eq!(t["comments"].as_array().unwrap().len(), 1);
            seen.push(t["id"].as_str().unwrap().to_string());
        }
        pages += 1;
        match page["next"].as_str() {
            Some(n) => after = n.to_string(),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let mut want = made.clone();
    want.sort();
    seen.sort();
    assert_eq!(seen, want);
    let (status, _) = send(&h.app, Method::GET, "/c/nope/threads", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &h.app,
        Method::GET,
        &format!("/c/{id}/threads?limit=0"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// POST a thread with highlight columns; returns (status, body, the marks header).
async fn new_highlight(
    h: &Harness,
    id: &str,
    version: i64,
    lines: (u32, u32),
    cols: (u32, u32),
) -> (StatusCode, String, Option<Value>) {
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/c/{id}/threads"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"version_id": version, "first_line": lines.0, "last_line": lines.1,
                   "start_col": cols.0, "end_col": cols.1, "body": "these words"})
            .to_string(),
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let marks = resp
        .headers()
        .get("x-margin-marks")
        .map(|v| serde_json::from_str(v.to_str().unwrap()).unwrap());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap(), marks)
}

/// A one-document collection of Rust source; (collection id, version id).
async fn rust_collection(h: &Harness, text: &str) -> (String, i64) {
    let created = post(&h.app, "t", vec![doc("a.rs", text)]).await;
    let id = created["id"].as_str().unwrap().to_string();
    let (_, body) = send(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
    )
    .await;
    let got: Value = serde_json::from_str(&body).unwrap();
    (id, got["documents"][0]["version_id"].as_i64().unwrap())
}

/// The inner HTML of line `n`'s code cell on the reader page.
fn code_of(page: &str, n: usize) -> String {
    let row = page.split(&format!("data-line=\"{n}\"")).nth(1).unwrap();
    let code = row.split("<span class=\"code\">").nth(1).unwrap();
    code.split("</span></div>").next().unwrap().to_string()
}

fn text_of(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The text of each `hl` run in `html`, in order, with its opening tag.
fn runs_of(html: &str) -> Vec<(String, String)> {
    html.split("<span class=\"hl ")
        .skip(1)
        .map(|rest| {
            let tag_end = rest.find('>').unwrap();
            let open = rest[..tag_end].to_string();
            let mut depth = 1;
            let mut at = tag_end + 1;
            let body_start = at;
            while depth > 0 {
                let next_open = rest[at..].find("<span").map(|p| p + at);
                let next_close = rest[at..].find("</span>").map(|p| p + at).unwrap();
                match next_open {
                    Some(o) if o < next_close => {
                        depth += 1;
                        at = o + 5;
                    }
                    _ => {
                        depth -= 1;
                        if depth == 0 {
                            return (open, rest[body_start..next_close].to_string());
                        }
                        at = next_close + 7;
                    }
                }
            }
            unreachable!()
        })
        .collect()
}

#[test_id(tag9kvn)]
#[tokio::test]
async fn posting_columns_stores_them_and_the_page_wraps_exactly_those_characters() {
    let h = harness();
    let (id, v) = rust_collection(&h, "fn main() {\n    let s = \"a<b\";\n}\n").await;
    // "main() {" on line 1: columns 3..11.
    let (status, _, marks) = new_highlight(&h, &id, v, (1, 1), (3, 11)).await;
    assert_eq!(status, StatusCode::CREATED);
    let marks = marks.unwrap();
    assert_eq!(marks[0]["start_col"], 3);
    assert_eq!(marks[0]["end_col"], 11);
    let (_, listing) = send(&h.app, Method::GET, &format!("/c/{id}/threads"), None, None).await;
    let listing: Value = serde_json::from_str(&listing).unwrap();
    assert_eq!(listing["items"][0]["start_col"], 3);
    assert_eq!(listing["items"][0]["end_col"], 11);

    let (_, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    let line1 = code_of(&page, 1);
    assert_eq!(
        text_of(&line1),
        "fn main() {",
        "the line's text is untouched"
    );
    let runs = runs_of(&line1);
    assert_eq!(runs.len(), 1, "{line1}");
    assert_eq!(text_of(&runs[0].1), "main() {");
    assert!(runs[0].0.starts_with("hl-l0\""), "{}", runs[0].0);
    assert!(
        runs[0].1.contains("<span class=\""),
        "syntax spans survive inside the run: {}",
        runs[0].1
    );
    assert!(
        runs[0].1.contains("entity name function"),
        "the function name keeps its syntect classes: {}",
        runs[0].1
    );
    assert!(code_of(&page, 2).find("hl-l").is_none());
}

#[test_id(t4mz0sd)]
#[tokio::test]
async fn a_highlight_across_lines_and_an_overlapping_one_each_keep_their_words_and_lanes() {
    let h = harness();
    let (id, v) = rust_collection(&h, "let a = 1;\nlet b = \"x & y\";\nlet c = 3;\n").await;
    // From "a = 1;" on line 1 (col 4) through "let b" on line 2 (end col 5).
    assert_eq!(
        new_highlight(&h, &id, v, (1, 2), (4, 5)).await.0,
        StatusCode::CREATED
    );
    // "b = \"x" on line 2, columns 4..10: overlaps the first on "b" only.
    assert_eq!(
        new_highlight(&h, &id, v, (2, 2), (4, 10)).await.0,
        StatusCode::CREATED
    );
    let (_, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    let first: Vec<String> = runs_of(&code_of(&page, 1))
        .iter()
        .map(|(_, t)| text_of(t))
        .collect();
    assert_eq!(first, ["a = 1;"]);
    let second = runs_of(&code_of(&page, 2));
    let shown: Vec<(String, String)> = second
        .iter()
        .map(|(open, t)| (open.split('"').next().unwrap().to_string(), text_of(t)))
        .collect();
    assert_eq!(
        shown,
        [
            ("hl-l0".to_string(), "let ".to_string()),
            ("hl-l0 hl-l1".to_string(), "b".to_string()),
            ("hl-l1".to_string(), " = \"x".to_string())
        ],
        "the shared word carries both lanes"
    );
    // A third on "let" shares those words with the first.
    assert_eq!(
        new_highlight(&h, &id, v, (2, 2), (0, 3)).await.0,
        StatusCode::CREATED
    );
    let (_, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    let both: Vec<(String, String)> = runs_of(&code_of(&page, 2))
        .iter()
        .map(|(open, t)| (open.split('"').next().unwrap().to_string(), text_of(t)))
        .collect();
    assert_eq!(both[0], ("hl-l0 hl-l2".to_string(), "let".to_string()));
    assert_eq!(both[1], ("hl-l0".to_string(), " ".to_string()));
    assert_eq!(both[2], ("hl-l0 hl-l1".to_string(), "b".to_string()));
    // Line 3 is untouched.
    assert!(!code_of(&page, 3).contains("hl-l"));
}

#[test_id(tq3p7yw)]
#[tokio::test]
async fn out_of_range_or_half_given_columns_answer_400() {
    let h = harness();
    let (id, v) = rust_collection(&h, "fn main() {}\n").await;
    for cols in [(0, 13), (5, 2), (4, 4)] {
        let (status, body, _) = new_highlight(&h, &id, v, (1, 1), cols).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{cols:?}: {body}");
    }
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/c/{id}/threads"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"version_id": v, "first_line": 1, "last_line": 1, "start_col": 2, "body": "x"})
                .to_string(),
        ))
        .unwrap();
    let resp = h.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
