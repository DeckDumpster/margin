//! The change feed and held comments, over the server in-process and a temporary database.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use margin::agents::hash_token;
use margin::api::{AppState, router};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::{Event, Metrics};
use margin_core::{Clock, Rng};
use margin_test_id::test_id;
use serde_json::{Value, json};
use tower::ServiceExt;

const ADA: &str = "ada-token-0123456789";
const BOB: &str = "bob-token-0123456789";

/// A clock the test moves by hand.
struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_unix(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

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
    clock: Arc<TestClock>,
    events: std::sync::mpsc::Receiver<Event>,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join("agents.toml");
    std::fs::write(
        &agents,
        format!(
            "ada = \"{}\"\nbob = \"{}\"\n",
            hash_token(ADA),
            hash_token(BOB)
        ),
    )
    .unwrap();
    let config = Config::parse(&format!(
        "listen = \"127.0.0.1:0\"\nbase_url = \"http://margin.test:7340/\"\ndatabase = \"{}\"\nagents_file = \"{}\"\notlp_endpoint = \"http://127.0.0.1:1/v1/metrics\"\ninstance = \"test\"\n",
        dir.path().join("m.db").display(),
        agents.display()
    ))
    .unwrap();
    let clock = Arc::new(TestClock(AtomicI64::new(1_700_000_000)));
    let (metrics, events) = Metrics::capture();
    let state = AppState {
        db: Db::open(&config.database).unwrap(),
        config: Arc::new(config),
        clock: clock.clone(),
        rng: Arc::new(CountingRng(AtomicU64::new(1))),
        metrics,
        highlighter: Arc::new(Highlighter::new()),
    };
    Harness {
        app: router(state),
        clock,
        events,
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

async fn json_of(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
    want: StatusCode,
) -> Value {
    let (status, text) = send(app, method, uri, token, body).await;
    assert_eq!(status, want, "{text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// (collection id, version id) of a ten-line document at `a.txt`.
async fn collection(h: &Harness) -> (String, i64) {
    let text: String = (1..=10).map(|n| format!("line {n}\n")).collect();
    let made = json_of(
        &h.app,
        Method::POST,
        "/api/collections",
        Some(ADA),
        Some(json!({"title": "t", "documents": [{"path": "a.txt", "kind": "code", "text": text}]})),
        StatusCode::CREATED,
    )
    .await;
    let id = made["id"].as_str().unwrap().to_string();
    let got = json_of(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
        StatusCode::OK,
    )
    .await;
    (id, got["documents"][0]["version_id"].as_i64().unwrap())
}

async fn agent_thread(h: &Harness, token: &str, id: &str, body: &str, held: bool) -> Value {
    json_of(
        &h.app,
        Method::POST,
        &format!("/api/collections/{id}/threads"),
        Some(token),
        Some(json!({"path": "a.txt", "first_line": 2, "last_line": 3, "body": body, "held": held})),
        StatusCode::CREATED,
    )
    .await
}

async fn reader_thread(h: &Harness, id: &str, version: i64, body: &str) {
    let (status, text) = send(
        &h.app,
        Method::POST,
        &format!("/c/{id}/threads"),
        None,
        Some(json!({"version_id": version, "first_line": 5, "last_line": 5, "body": body})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{text}");
}

async fn last_seq(h: &Harness) -> i64 {
    json_of(
        &h.app,
        Method::GET,
        "/api/changes?after=0",
        Some(ADA),
        None,
        StatusCode::OK,
    )
    .await["last_seq"]
        .as_i64()
        .unwrap()
}

#[test_id(tqi1a9a)]
#[tokio::test]
async fn a_reader_comment_reaches_a_waiting_agent_within_a_second() {
    let h = harness();
    let (id, version) = collection(&h).await;
    let after = last_seq(&h).await;
    let app = h.app.clone();
    let started = Instant::now();
    let poll = tokio::spawn(async move {
        json_of(
            &app,
            Method::GET,
            &format!("/api/changes?after={after}&wait=30"),
            Some(ADA),
            None,
            StatusCode::OK,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    reader_thread(&h, &id, version, "from the reader").await;
    let got = poll.await.unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    let rows = got["changes"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{got}");
    assert_eq!(rows[0]["kind"], "thread_created");
    assert_eq!(rows[0]["author"], "reader");
    assert_eq!(rows[0]["visibility"], "now");
    assert_eq!(rows[0]["body"], "from the reader");
    assert_eq!(got["last_seq"], rows[0]["seq"]);
}

#[test_id(tl3gnm3)]
#[tokio::test]
async fn a_poll_with_nothing_new_answers_empty_when_its_wait_ends() {
    let h = harness();
    collection(&h).await;
    let after = last_seq(&h).await;
    let started = Instant::now();
    let got = json_of(
        &h.app,
        Method::GET,
        &format!("/api/changes?after={after}&wait=1"),
        Some(ADA),
        None,
        StatusCode::OK,
    )
    .await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(2),
        "{took:?}"
    );
    assert_eq!(got["changes"], json!([]));
    assert_eq!(got["last_seq"], after);
}

#[test_id(tqj7ktn)]
#[tokio::test]
async fn a_feed_call_is_held_past_the_three_second_deadline_every_other_route_keeps() {
    let h = harness();
    collection(&h).await;
    let after = last_seq(&h).await;
    let started = Instant::now();
    let (status, _) = send(
        &h.app,
        Method::GET,
        &format!("/api/changes?after={after}&wait=4"),
        Some(ADA),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
}

#[test_id(tsraks2)]
#[tokio::test]
async fn rows_after_n_answer_at_once_at_most_a_hundred_at_a_time() {
    let h = harness();
    let (id, _) = collection(&h).await;
    let first = agent_thread(&h, ADA, &id, "one", false).await;
    let tid = first["id"].as_str().unwrap();
    for n in 0..104 {
        json_of(
            &h.app,
            Method::POST,
            &format!("/api/threads/{tid}/comments"),
            Some(ADA),
            Some(json!({"body": format!("r{n}")})),
            StatusCode::CREATED,
        )
        .await;
    }
    let started = Instant::now();
    let page = json_of(
        &h.app,
        Method::GET,
        "/api/changes?after=0&wait=30",
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(page["changes"].as_array().unwrap().len(), 100);
    let more = json_of(
        &h.app,
        Method::GET,
        &format!("/api/changes?after={}", page["last_seq"]),
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(more["changes"].as_array().unwrap().len(), 5, "{more}");
}

#[test_id(tc7dw54)]
#[tokio::test]
async fn an_agents_held_comment_is_in_the_agent_feed_not_the_page_until_publish() {
    let h = harness();
    let (id, version) = collection(&h).await;
    reader_thread(&h, &id, version, "mine").await;
    let held = agent_thread(&h, ADA, &id, "secret draft", true).await;
    assert_eq!(held["comments"][0]["author"], "ada");
    assert_eq!(held["comments"][0]["visibility"], "held");

    let (status, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        page.contains("mine") && !page.contains("secret draft"),
        "held text on the page"
    );
    let agent_feed = json_of(
        &h.app,
        Method::GET,
        "/api/changes?after=0",
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    assert!(
        agent_feed["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["body"] == "secret draft" && c["visibility"] == "held")
    );
    let page_feed = json_of(
        &h.app,
        Method::GET,
        &format!("/c/{id}/changes?after=0"),
        None,
        None,
        StatusCode::OK,
    )
    .await;
    assert!(
        !page_feed.to_string().contains("secret draft"),
        "{page_feed}"
    );
    assert_eq!(page_feed["threads"].as_array().unwrap().len(), 1);
    let (_, listed) = send(&h.app, Method::GET, &format!("/c/{id}/threads"), None, None).await;
    assert!(!listed.contains("secret draft"));
    let tid = held["id"].as_str().unwrap();
    let (status, _) = send(
        &h.app,
        Method::POST,
        &format!("/c/{id}/threads/{tid}/comments"),
        None,
        Some(json!({"body": "peek"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the reader cannot reply into a thread he cannot see"
    );

    let seen = page_feed["last_seq"].as_i64().unwrap();
    let done = json_of(
        &h.app,
        Method::POST,
        &format!("/c/{id}/publish"),
        None,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(done["released"], 1);

    let (_, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    assert!(page.contains("mine") && page.contains("secret draft"));
    let collection = json_of(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}"),
        None,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(collection["state"], "published");
    let page_feed = json_of(
        &h.app,
        Method::GET,
        &format!("/c/{id}/changes?after={seen}"),
        None,
        None,
        StatusCode::OK,
    )
    .await;
    assert!(
        page_feed.to_string().contains("secret draft"),
        "{page_feed}"
    );
    let agent_feed = json_of(
        &h.app,
        Method::GET,
        &format!("/api/changes?after={seen}"),
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    let kinds: Vec<&str> = agent_feed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "published").count(),
        1,
        "{kinds:?}"
    );
}

#[test_id(tb8x4fj)]
#[tokio::test]
async fn publish_is_refused_on_a_closed_collection_and_a_held_reply_stays_held_in_a_visible_thread()
{
    let h = harness();
    let (id, version) = collection(&h).await;
    reader_thread(&h, &id, version, "mine").await;
    let page_feed = json_of(
        &h.app,
        Method::GET,
        &format!("/c/{id}/changes?after=0"),
        None,
        None,
        StatusCode::OK,
    )
    .await;
    let tid = page_feed["threads"][0]["id"].as_str().unwrap().to_string();
    json_of(
        &h.app,
        Method::POST,
        &format!("/api/threads/{tid}/comments"),
        Some(ADA),
        Some(json!({"body": "held reply", "held": true})),
        StatusCode::CREATED,
    )
    .await;
    let seen = last_seq(&h).await;
    // Only a held comment changed: the page's poll has nothing to say.
    let quiet = json_of(
        &h.app,
        Method::GET,
        &format!("/c/{id}/changes?after={}&wait=1", seen - 1),
        None,
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(quiet["threads"], json!([]));
    let thread = json_of(
        &h.app,
        Method::GET,
        &format!("/api/threads/{tid}"),
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(thread["comments"].as_array().unwrap().len(), 2);

    json_of(
        &h.app,
        Method::PATCH,
        &format!("/api/collections/{id}"),
        Some(ADA),
        Some(json!({"state": "closed"})),
        StatusCode::OK,
    )
    .await;
    let (status, _) = send(
        &h.app,
        Method::POST,
        &format!("/c/{id}/publish"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (_, page) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    assert!(
        !page.contains("held reply")
            && !page.contains("id=\"publish\"\n")
            && page.contains("id=\"publish\" hidden")
    );
}

#[test_id(t92qgh1)]
#[tokio::test]
async fn an_agent_changes_and_deletes_only_its_own_comments() {
    let h = harness();
    let (id, _) = collection(&h).await;
    let made = agent_thread(&h, ADA, &id, "ada's", false).await;
    let cid = made["comments"][0]["id"].as_str().unwrap().to_string();
    let uri = format!("/api/comments/{cid}");
    let (status, _) = send(
        &h.app,
        Method::PATCH,
        &uri,
        Some(BOB),
        Some(json!({"body": "hijack"})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&h.app, Method::DELETE, &uri, Some(BOB), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let edited = json_of(
        &h.app,
        Method::PATCH,
        &uri,
        Some(ADA),
        Some(json!({"body": "edited"})),
        StatusCode::OK,
    )
    .await;
    assert_eq!(edited["comments"][0]["body"], "edited");
    let (status, _) = send(&h.app, Method::DELETE, &uri, Some(ADA), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let feed = json_of(
        &h.app,
        Method::GET,
        "/api/changes?after=0",
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    let kinds: Vec<&str> = feed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        ["thread_created", "comment_updated", "comment_deleted"]
    );
    assert_eq!(
        feed["changes"][2]["author"], "ada",
        "a deleted comment keeps its author in the log"
    );
}

#[test_id(th5l22j)]
#[tokio::test]
async fn a_bad_or_missing_token_answers_401_on_every_agent_route() {
    let h = harness();
    let (id, _) = collection(&h).await;
    let routes = [
        (Method::GET, "/api/changes?after=0".to_string()),
        (Method::GET, format!("/api/collections/{id}/threads")),
        (Method::POST, format!("/api/collections/{id}/threads")),
        (Method::GET, "/api/threads/x".to_string()),
        (Method::POST, "/api/threads/x/comments".to_string()),
        (Method::PATCH, "/api/comments/x".to_string()),
        (Method::DELETE, "/api/comments/x".to_string()),
    ];
    for (method, uri) in routes {
        for token in [Some("wrong-token"), None] {
            let (status, _) = send(&h.app, method.clone(), &uri, token, Some(json!({}))).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        }
    }
}

#[test_id(t2g08x5)]
#[tokio::test]
async fn the_agent_thread_list_is_paged_in_sql_and_includes_held_threads() {
    let h = harness();
    let (id, _) = collection(&h).await;
    for n in 0..3 {
        h.clock.0.fetch_add(1, Ordering::SeqCst);
        agent_thread(&h, ADA, &id, &format!("t{n}"), n == 1).await;
    }
    let first = json_of(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}/threads?limit=2"),
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let next = first["next"].as_str().unwrap();
    let rest = json_of(
        &h.app,
        Method::GET,
        &format!("/api/collections/{id}/threads?limit=2&after={next}"),
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest["next"], Value::Null);
}

#[test_id(tvkprhq)]
#[tokio::test]
async fn fifty_waiting_polls_do_not_hold_up_a_page_get() {
    let h = harness();
    let (id, version) = collection(&h).await;
    reader_thread(&h, &id, version, "mine").await;
    let after = last_seq(&h).await;
    let polls: Vec<_> = (0..50)
        .map(|_| {
            let app = h.app.clone();
            tokio::spawn(async move {
                send(
                    &app,
                    Method::GET,
                    &format!("/api/changes?after={after}&wait=30"),
                    Some(ADA),
                    None,
                )
                .await
            })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = Instant::now();
    let (status, _) = send(&h.app, Method::GET, &format!("/c/{id}"), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    // And one write wakes all fifty.
    reader_thread(&h, &id, version, "wake").await;
    for p in polls {
        let (status, body) = p.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("wake"));
    }
}

#[test_id(t80hxpd)]
#[tokio::test]
async fn comments_and_feed_calls_push_their_metrics() {
    let h = harness();
    let (id, _) = collection(&h).await;
    agent_thread(&h, ADA, &id, "held one", true).await;
    h.clock.0.fetch_add(3, Ordering::SeqCst);
    json_of(
        &h.app,
        Method::GET,
        "/api/changes?after=0",
        Some(BOB),
        None,
        StatusCode::OK,
    )
    .await;
    let pushed: Vec<Event> = h.events.try_iter().collect();
    assert!(
        pushed.iter().any(|e| matches!(
            e,
            Event::Comment {
                author_kind: "agent",
                visibility: "held"
            }
        )),
        "{pushed:?}"
    );
    assert!(pushed.iter().any(|e| matches!(e, Event::FeedWait { .. })));
    assert!(
        pushed
            .iter()
            .any(|e| matches!(e, Event::FeedLag { seconds } if *seconds == 3.0)),
        "{pushed:?}"
    );
}

#[test_id(tol26g5)]
#[tokio::test]
async fn a_wait_of_six_hundred_is_held_no_longer_than_thirty_seconds() {
    let h = harness();
    collection(&h).await;
    let after = last_seq(&h).await;
    // From here the clock is the runtime's: with nothing else pending, an idle runtime jumps
    // straight to the one timer the feed call set, so the call's own bound shows as elapsed time.
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let got = json_of(
        &h.app,
        Method::GET,
        &format!("/api/changes?after={after}&wait=600"),
        Some(ADA),
        None,
        StatusCode::OK,
    )
    .await;
    let held = started.elapsed();
    assert!(
        held >= Duration::from_secs(30) && held < Duration::from_secs(31),
        "{held:?}"
    );
    assert_eq!(got["changes"], json!([]));
}
