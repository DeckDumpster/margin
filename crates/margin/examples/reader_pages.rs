//! Times the reader route on a review of the shape of review 002: 65 Rust documents, 38,247
//! lines, the largest 5,910. `api::READER_DEADLINE` is set from what this prints.
//!
//! `cargo run --release -p margin --example reader_pages`
//!
//! Warm times render every page 20 times on one server whose cache already holds the review;
//! the first of them records every version's definitions in the store. Cold times are a page's
//! first render on a fresh router and `Highlighter` over that store: a restart, which parses
//! only the page's own documents. The router cuts a render off at its deadline, so the cold
//! renders are timed again by calling `Refs::new` and `page::render` themselves.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use margin::agents::OsRng;
use margin::api::{AppState, router};
use margin::config::Config;
use margin::db::Db;
use margin::highlight::Highlighter;
use margin::metrics::Metrics;
use margin::refs::Refs;
use margin::store::{Collection, Kind, NewCollection, NewDocument};
use margin_core::SystemClock;
use tower::ServiceExt;

/// Real Rust, cycled through to fill the documents.
const SOURCE: &str = include_str!("../../margin-ui/fixtures/long.rs");
const PAGE_LINES: usize = 1500;
const WARM: usize = 20;
/// Of 20 runs, the p99 is the slowest.
const COLD: usize = 20;
/// Longer than the renders the cold runs leave behind take to finish.
const SETTLE: Duration = Duration::from_secs(5);

/// The documents' line counts: the three largest of review 002 among 62 that share the rest.
fn shape() -> Vec<usize> {
    let mut lines = vec![455; 61];
    lines.push(453);
    for (at, large) in [(20, 5910), (35, 2067), (50, 2062)] {
        lines.insert(at, large);
    }
    assert_eq!((lines.len(), lines.iter().sum()), (65, 38_247));
    lines
}

fn server(db: &Db, config: &Arc<Config>, highlighter: Arc<Highlighter>) -> Router {
    router(AppState {
        db: db.clone(),
        config: config.clone(),
        clock: Arc::new(SystemClock),
        rng: Arc::new(OsRng),
        metrics: Metrics::detached(),
        highlighter,
    })
}

/// One render of page `page`: how long until the whole body, and the answer's status.
async fn render(app: &Router, page: usize) -> (Duration, StatusCode) {
    let request = Request::builder()
        .uri(format!("/c/review?page={page}"))
        .body(Body::empty())
        .unwrap();
    let started = Instant::now();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    response.into_body().collect().await.unwrap();
    (started.elapsed(), status)
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `p50 …  max …` of the times, naming any answer that was not 200.
fn report(mut runs: Vec<(Duration, StatusCode)>) -> String {
    runs.sort_by_key(|(d, _)| *d);
    let failed: Vec<u16> = runs
        .iter()
        .filter(|(_, s)| *s != StatusCode::OK)
        .map(|(_, s)| s.as_u16())
        .collect();
    let mut out = format!(
        "p50 {:7.1} ms  max {:7.1} ms",
        ms(runs[runs.len() / 2].0),
        ms(runs[runs.len() - 1].0)
    );
    if !failed.is_empty() {
        out.push_str(&format!("  answered {failed:?}"));
    }
    out
}

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let config = Arc::new(
        Config::parse(&format!(
            "listen = \"127.0.0.1:0\"\nbase_url = \"http://margin.test\"\ndatabase = \"{}\"\nagents_file = \"{}\"\notlp_endpoint = \"http://127.0.0.1:1\"\ninstance = \"measure\"\npage_lines = {PAGE_LINES}\n",
            dir.path().join("m.db").display(),
            dir.path().join("agents.toml").display()
        ))
        .unwrap(),
    );
    let db = Db::open(&config.database).unwrap();

    let shape = shape();
    let mut source = SOURCE.lines().cycle();
    let documents: Vec<(String, NewDocument)> = shape
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let text = source.by_ref().take(n).map(|l| format!("{l}\n")).collect();
            let document = NewDocument {
                path: format!("src/file{i}.rs"),
                kind: Kind::Code,
                text,
            };
            (format!("d{i}"), document)
        })
        .collect();
    db.call(move |conn| {
        margin::store::create(
            conn,
            &NewCollection {
                id: "review",
                title: "The shape of review 002",
                cover_note: "Measured.",
                created_by: "measure",
                now: 0,
                source: None,
                documents: &documents,
            },
        )
    })
    .await
    .unwrap()
    .unwrap();

    let pages = margin_core::pages::split(&shape, PAGE_LINES);
    let lines: Vec<usize> = pages
        .iter()
        .map(|p| shape[p.clone()].iter().sum())
        .collect();
    let largest = 1 + (0..lines.len()).max_by_key(|&i| lines[i]).unwrap();
    println!(
        "{} documents, {} lines, {} pages of up to {PAGE_LINES}; the largest is page {largest}",
        shape.len(),
        shape.iter().sum::<usize>(),
        pages.len()
    );

    let load = async || {
        db.call(|conn| margin::store::get(conn, "review"))
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    };
    let unrecorded = load().await;
    // A render with no router around it: no deadline cuts it off.
    let uncut = |collection: &Collection, highlighter: &Highlighter, page: usize| {
        let started = Instant::now();
        let refs = Refs::new(collection, highlighter);
        margin::page::render(collection, &[], 0, highlighter, PAGE_LINES, page, &refs)
            .unwrap()
            .unwrap();
        (started.elapsed(), StatusCode::OK)
    };

    println!("warm, {WARM} renders a page:");
    let highlighter = Arc::new(Highlighter::new());
    // Fills the cache outside any deadline; the router's first render then records.
    uncut(&unrecorded, &highlighter, 1);
    let app = server(&db, &config, highlighter);
    for (i, n) in lines.iter().enumerate() {
        let mut runs = Vec::new();
        for _ in 0..WARM {
            runs.push(render(&app, i + 1).await);
        }
        println!("  page {:2}  {n:5} lines  {}", i + 1, report(runs));
    }

    let recorded = load().await;
    assert!(
        recorded.documents.iter().all(|d| d.definitions.is_some()),
        "the first render through the router recorded every version"
    );

    println!("cold, {COLD} first renders, each on a fresh router and highlighter:");
    for page in [1, largest] {
        let mut runs = Vec::new();
        for _ in 0..COLD {
            let app = server(&db, &config, Arc::new(Highlighter::new()));
            runs.push(render(&app, page).await);
        }
        println!(
            "  page {page:2}  {:5} lines  {}",
            lines[page - 1],
            report(runs)
        );
    }

    // A render the router cut off above is still running; let it finish before timing more.
    tokio::time::sleep(SETTLE).await;
    println!("cold, the same renders with no deadline:");
    for page in [1, largest] {
        let runs = (0..COLD)
            .map(|_| uncut(&recorded, &Highlighter::new(), page))
            .collect();
        println!(
            "  page {page:2}  {:5} lines  {}",
            lines[page - 1],
            report(runs)
        );
    }
}
