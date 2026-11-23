//! HTTP routes. Handlers only await: SQLite runs on the [`Db`] workers and
//! highlighting on the blocking pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, MatchedPath, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use margin_core::{Clock, Rng};
use serde::{Deserialize, Serialize};
use tower_http::timeout::TimeoutLayer;

use crate::changes::{self, View};
use crate::config::Config;
use crate::db::{Db, DbError};
use crate::feed::{self, Probe};
use crate::highlight::Highlighter;
use crate::metrics::{Event, Metrics};
use crate::store::{self, NewCollection, NewDocument, Patch};
use crate::threads::{
    self, Change, CreateError, Deleted, Mark, NewThread, Published, Scope, Thread, Visibility,
};
use crate::versions;
use askama::Template;
use margin_core::anchor::Anchor;

/// Every non-feed route answers within this or is cut off.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(3);
const BODY_LIMIT: usize = 16 * 1024 * 1024;
const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 200;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub config: Arc<Config>,
    pub clock: Arc<dyn Clock>,
    pub rng: Arc<dyn Rng>,
    pub metrics: Metrics,
    pub highlighter: Arc<Highlighter>,
}

pub fn router(state: AppState) -> Router {
    // Every route but the two feeds answers within REQUEST_DEADLINE. A feed call is held open
    // on purpose, up to `feed::MAX_WAIT`, and bounds itself.
    let bounded = Router::new()
        .route("/api/collections", get(list).post(create))
        .route(
            "/api/collections/{id}",
            get(read).patch(update).delete(remove),
        )
        .route(
            "/api/collections/{id}/threads",
            get(agent_list_threads).post(agent_create_thread),
        )
        .route(
            "/api/documents/{doc_id}/versions",
            get(list_versions).post(create_version),
        )
        .route(
            "/api/documents/{doc_id}/versions/{n}",
            get(read_version).delete(delete_version),
        )
        .route("/api/threads/{tid}", get(agent_thread))
        .route("/api/threads/{tid}/comments", post(agent_reply))
        .route(
            "/api/comments/{cid}",
            patch(agent_edit_comment).delete(agent_delete_comment),
        )
        .route("/c/{id}", get(reader))
        .route("/c/{id}/publish", post(publish))
        .route("/c/{id}/threads", get(list_threads).post(create_thread))
        .route("/c/{id}/threads/{tid}/comments", post(reply))
        .route(
            "/c/{id}/comments/{cid}",
            patch(edit_comment).delete(delete_comment),
        )
        .route("/healthz", get(healthz))
        .route(
            "/static/margin.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    crate::page::CSS,
                )
            }),
        )
        .route(
            "/static/margin.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    crate::page::JS,
                )
            }),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            REQUEST_DEADLINE,
        ));
    let feeds = Router::new()
        .route("/api/changes", get(agent_changes))
        .route("/c/{id}/changes", get(page_changes));
    bounded
        .merge(feeds)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(state.clone(), measure))
        .with_state(state)
}

async fn measure(State(s): State<AppState>, req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".into());
    let started = Instant::now();
    let resp = next.run(req).await;
    s.metrics.push(Event::Request {
        route,
        status: resp.status().as_u16(),
        seconds: started.elapsed().as_secs_f64(),
    });
    resp
}

pub enum ApiError {
    Unauthorized,
    Forbidden,
    Conflict(String),
    NotFound,
    BadRequest(String),
    Busy,
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "a valid bearer token is required".to_string(),
            ),
            ApiError::Forbidden => (
                StatusCode::FORBIDDEN,
                "only a comment's own author can change it".to_string(),
            ),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy, try again".into()),
            ApiError::Internal(m) => {
                eprintln!("margin: internal error: {m}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        let mut resp = (code, Json(serde_json::json!({ "error": msg }))).into_response();
        if code == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                "Bearer".parse().expect("static header"),
            );
        }
        resp
    }
}

impl AppState {
    fn db_error(&self, e: DbError) -> ApiError {
        match e {
            DbError::QueueFull => {
                eprintln!("margin: database queue is full; answering 503");
                self.metrics.push(Event::QueueFull);
                ApiError::Busy
            }
            other => ApiError::Internal(other.to_string()),
        }
    }

    /// Record how long a feed call was held and how old each change in its answer was.
    fn feed_answered(&self, started: Instant, rows: &[changes::Row]) {
        self.metrics.push(Event::FeedWait {
            seconds: started.elapsed().as_secs_f64(),
        });
        let now = self.clock.now_unix();
        for r in rows {
            self.metrics.push(Event::FeedLag {
                seconds: (now - r.at).max(0) as f64,
            });
        }
    }

    fn comment_written(&self, author: &str, visibility: Visibility) {
        self.metrics.push(Event::Comment {
            author_kind: if author == threads::READER {
                "reader"
            } else {
                "agent"
            },
            visibility: visibility.as_str(),
        });
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        match self.db.call(f).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(ApiError::Internal(e.to_string())),
            Err(e) => Err(self.db_error(e)),
        }
    }

    /// The agent named by the request's bearer token, per the agents file as it is now.
    async fn agent(&self, headers: &HeaderMap) -> Result<String, ApiError> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
            .ok_or(ApiError::Unauthorized)?;
        let text = tokio::fs::read_to_string(&self.config.agents_file)
            .await
            .map_err(|e| ApiError::Internal(format!("reading agents file: {e}")))?;
        crate::agents::authenticate(&text, token).ok_or(ApiError::Unauthorized)
    }
}

#[derive(Deserialize)]
struct CreateBody {
    title: String,
    #[serde(default)]
    cover_note: String,
    documents: Vec<NewDocument>,
}

#[derive(Serialize)]
struct Created {
    id: String,
    url: String,
}

async fn create(
    State(s): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateBody>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let agent = s.agent(&headers).await?;
    let Json(body) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    if body.title.trim().is_empty() {
        return Err(ApiError::BadRequest("title must not be empty".into()));
    }
    if body.documents.is_empty() {
        return Err(ApiError::BadRequest(
            "a collection needs at least one document".into(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for d in &body.documents {
        if d.path.trim().is_empty() {
            return Err(ApiError::BadRequest(
                "document path must not be empty".into(),
            ));
        }
        if !seen.insert(d.path.as_str()) {
            return Err(ApiError::BadRequest(format!(
                "duplicate document path {}",
                d.path
            )));
        }
    }
    let id = margin_core::new_id(&*s.rng);
    let documents: Vec<(String, NewDocument)> = body
        .documents
        .into_iter()
        .map(|d| (margin_core::new_id(&*s.rng), d))
        .collect();
    let now = s.clock.now_unix();
    let (id2, title, cover) = (id.clone(), body.title, body.cover_note);
    s.run(move |conn| {
        store::create(
            conn,
            &NewCollection {
                id: &id2,
                title: &title,
                cover_note: &cover,
                created_by: &agent,
                now,
                documents: &documents,
            },
        )
    })
    .await?;
    let url = format!("{}/c/{}", s.config.base_url, id);
    Ok((StatusCode::CREATED, Json(Created { id, url })))
}

async fn read(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<store::Collection>, ApiError> {
    let found = s.run(move |conn| store::get(conn, &id)).await?;
    found.map(Json).ok_or(ApiError::NotFound)
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<usize>,
    after: Option<String>,
}

#[derive(Serialize)]
struct Listing {
    items: Vec<store::Summary>,
    next: Option<String>,
}

fn cursor(s: &store::Summary) -> String {
    format!("{}.{}", s.created_at, s.id)
}

async fn list(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Listing>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(ApiError::BadRequest(format!(
            "limit must be 1..={MAX_PAGE}"
        )));
    }
    let after = match q.after {
        None => None,
        Some(c) => Some(parse_cursor(&c)?),
    };
    let mut items = s
        .run(move |conn| {
            store::list(
                conn,
                limit + 1,
                after.as_ref().map(|(a, i)| (*a, i.as_str())),
            )
        })
        .await?;
    let next = if items.len() > limit {
        items.truncate(limit);
        items.last().map(cursor)
    } else {
        None
    };
    Ok(Json(Listing { items, next }))
}

async fn update(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Patch>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<store::Collection>, ApiError> {
    s.agent(&headers).await?;
    let Json(p) = body.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    if p.title.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err(ApiError::BadRequest("title must not be empty".into()));
    }
    let now = s.clock.now_unix();
    let id2 = id.clone();
    let found = s
        .run(move |conn| {
            if !store::patch(conn, &id2, &p, now)? {
                return Ok(None);
            }
            store::get(conn, &id2)
        })
        .await?;
    found.map(Json).ok_or(ApiError::NotFound)
}

async fn remove(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    s.agent(&headers).await?;
    if s.run(move |conn| store::delete(conn, &id)).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

async fn reader(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, ApiError> {
    // The sequence number is read before the threads, so a change that lands in between is
    // delivered again by the feed rather than missed.
    let (seq, collection, all_threads) = s
        .run(move |conn| {
            let seq = changes::newest(conn)?;
            Ok(match store::get(conn, &id)? {
                Some(c) => Some((seq, threads::all(conn, Scope::Reader, &id)?, c)),
                None => None,
            })
        })
        .await?
        .map(|(seq, t, c)| (seq, c, t))
        .ok_or(ApiError::NotFound)?;
    let highlighter = s.highlighter.clone();
    let started = Instant::now();
    let html = tokio::task::spawn_blocking(move || {
        crate::page::render(&collection, &all_threads, seq, &highlighter)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    s.metrics.push(Event::Render {
        seconds: started.elapsed().as_secs_f64(),
    });
    Ok(Html(html))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "sha": crate::BUILD_SHA }))
}

fn clean_body(body: &str) -> Result<String, ApiError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(ApiError::BadRequest("body must not be empty".into()));
    }
    if body.len() > threads::MAX_BODY {
        return Err(ApiError::BadRequest(format!(
            "body must be at most {} bytes",
            threads::MAX_BODY
        )));
    }
    Ok(body.to_string())
}

fn json_body<T>(
    body: Result<Json<T>, axum::extract::rejection::JsonRejection>,
) -> Result<T, ApiError> {
    body.map(|Json(b)| b)
        .map_err(|e| ApiError::BadRequest(e.body_text()))
}

/// The server-rendered thread, open, with the version's marks in `x-margin-marks` when the
/// write changed them (a thread appeared or went).
fn fragment(
    thread: &Thread,
    marks: Option<&[Mark]>,
    status: StatusCode,
) -> Result<Response, ApiError> {
    let html = crate::page::ThreadFragment {
        thread: crate::page::ThreadView::new(thread, true),
    }
    .render()
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(with_marks((status, Html(html)).into_response(), marks))
}

fn with_marks(mut resp: Response, marks: Option<&[Mark]>) -> Response {
    if let Some(marks) = marks {
        let json = serde_json::to_string(marks).expect("marks serialize");
        resp.headers_mut().insert(
            "x-margin-marks",
            json.parse().expect("marks JSON is plain ASCII"),
        );
    }
    resp
}

#[derive(Deserialize)]
struct NewThreadBody {
    version_id: i64,
    first_line: u32,
    last_line: u32,
    /// Both or neither: the columns of a highlight of some words.
    start_col: Option<u32>,
    end_col: Option<u32>,
    body: String,
}

async fn create_thread(
    State(s): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<NewThreadBody>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let b = json_body(body)?;
    let text = clean_body(&b.body)?;
    let cols = match (b.start_col, b.end_col) {
        (Some(s), Some(e)) => Some((s, e)),
        (None, None) => None,
        _ => {
            return Err(ApiError::BadRequest(
                "start_col and end_col come together".into(),
            ));
        }
    };
    let (thread_id, comment_id) = (margin_core::new_id(&*s.rng), margin_core::new_id(&*s.rng));
    let now = s.clock.now_unix();
    let made = s
        .run(move |conn| {
            let made = threads::create_thread(
                conn,
                &NewThread {
                    thread_id: &thread_id,
                    comment_id: &comment_id,
                    collection_id: &id,
                    version_id: b.version_id,
                    anchor: Anchor {
                        first: b.first_line,
                        last: b.last_line,
                        cols,
                    },
                    author: threads::READER,
                    visibility: Visibility::Now,
                    body: &text,
                    now,
                },
            );
            match made {
                Ok(t) => Ok(Ok((threads::version_marks(conn, &id, t.version_id)?, t))),
                Err(CreateError::Db(e)) => Err(e),
                Err(other) => Ok(Err(other)),
            }
        })
        .await?;
    match made {
        Ok((marks, thread)) => {
            s.comment_written(threads::READER, Visibility::Now);
            fragment(&thread, Some(&marks), StatusCode::CREATED)
        }
        Err(CreateError::NoVersion) => Err(ApiError::NotFound),
        Err(CreateError::Anchor(e)) => Err(ApiError::BadRequest(e.to_string())),
        Err(CreateError::Db(_)) => unreachable!("database errors leave through run"),
    }
}

#[derive(Deserialize)]
struct BodyOnly {
    body: String,
}

async fn reply(
    State(s): State<AppState>,
    Path((id, tid)): Path<(String, String)>,
    body: Result<Json<BodyOnly>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let text = clean_body(&json_body(body)?.body)?;
    let comment_id = margin_core::new_id(&*s.rng);
    let now = s.clock.now_unix();
    let thread = s
        .run(move |conn| {
            threads::add_comment(
                conn,
                &id,
                &tid,
                &comment_id,
                threads::READER,
                Visibility::Now,
                &text,
                now,
            )
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    s.comment_written(threads::READER, Visibility::Now);
    fragment(&thread, None, StatusCode::CREATED)
}

async fn edit_comment(
    State(s): State<AppState>,
    Path((id, cid)): Path<(String, String)>,
    body: Result<Json<BodyOnly>, axum::extract::rejection::JsonRejection>,
) -> Result<Response, ApiError> {
    let text = clean_body(&json_body(body)?.body)?;
    let now = s.clock.now_unix();
    match s
        .run(move |conn| threads::edit_comment(conn, Some(&id), threads::READER, &cid, &text, now))
        .await?
    {
        Change::NotFound => Err(ApiError::NotFound),
        Change::NotYours => Err(ApiError::Forbidden),
        Change::Done(thread) => fragment(&thread, None, StatusCode::OK),
    }
}

/// 200 with the thread when comments remain; 204 with the new marks when the thread went.
async fn delete_comment(
    State(s): State<AppState>,
    Path((id, cid)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let now = s.clock.now_unix();
    match s
        .run(move |conn| threads::delete_comment(conn, Some(&id), threads::READER, &cid, now))
        .await?
    {
        Change::NotFound => Err(ApiError::NotFound),
        Change::NotYours => Err(ApiError::Forbidden),
        Change::Done(Deleted::Left(thread)) => fragment(&thread, None, StatusCode::OK),
        Change::Done(Deleted::Gone(marks)) => Ok(with_marks(
            StatusCode::NO_CONTENT.into_response(),
            Some(&marks),
        )),
    }
}

#[derive(Serialize)]
struct ThreadListing {
    items: Vec<Thread>,
    next: Option<String>,
}

async fn list_threads(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ThreadListing>, ApiError> {
    threads_listing(&s, Scope::Reader, id, q).await
}

async fn threads_listing(
    s: &AppState,
    scope: Scope,
    id: String,
    q: ListQuery,
) -> Result<Json<ThreadListing>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(ApiError::BadRequest(format!(
            "limit must be 1..={MAX_PAGE}"
        )));
    }
    let after = match q.after {
        None => None,
        Some(c) => Some(parse_cursor(&c)?),
    };
    let (known, mut items) = s
        .run(move |conn| {
            let known = store::get(conn, &id)?.is_some();
            let items = threads::page(
                conn,
                scope,
                &id,
                limit + 1,
                after.as_ref().map(|(a, i)| (*a, i.as_str())),
            )?;
            Ok((known, items))
        })
        .await?;
    if !known {
        return Err(ApiError::NotFound);
    }
    let next = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|t| format!("{}.{}", t.created_at, t.id))
    } else {
        None
    };
    Ok(Json(ThreadListing { items, next }))
}

fn parse_cursor(c: &str) -> Result<(i64, String), ApiError> {
    let bad = || ApiError::BadRequest("after is not a cursor".into());
    let (at, id) = c.split_once('.').ok_or_else(bad)?;
    Ok((at.parse().map_err(|_| bad())?, id.to_string()))
}

#[derive(Deserialize)]
struct AgentThreadBody {
    path: String,
    first_line: u32,
    last_line: u32,
    start_col: Option<u32>,
    end_col: Option<u32>,
    body: String,
    #[serde(default)]
    held: bool,
}

#[derive(Deserialize)]
struct AgentCommentBody {
    body: String,
    #[serde(default)]
    held: bool,
}

fn visibility(held: bool) -> Visibility {
    if held {
        Visibility::Held
    } else {
        Visibility::Now
    }
}

/// Start a thread on the latest version of the document at `path`.
async fn agent_create_thread(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<AgentThreadBody>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Thread>), ApiError> {
    let agent = s.agent(&headers).await?;
    let b = json_body(body)?;
    let text = clean_body(&b.body)?;
    if b.start_col.is_some() != b.end_col.is_some() {
        return Err(ApiError::BadRequest(
            "start_col and end_col come together".into(),
        ));
    }
    let anchor = Anchor {
        first: b.first_line,
        last: b.last_line,
        cols: b.start_col.zip(b.end_col),
    };
    let held = visibility(b.held);
    let (thread_id, comment_id) = (margin_core::new_id(&*s.rng), margin_core::new_id(&*s.rng));
    let now = s.clock.now_unix();
    let who = agent.clone();
    let made = s
        .run(move |conn| {
            let Some(version_id) = threads::latest_version(conn, &id, &b.path)? else {
                return Ok(Err(CreateError::NoVersion));
            };
            match threads::create_thread(
                conn,
                &NewThread {
                    thread_id: &thread_id,
                    comment_id: &comment_id,
                    collection_id: &id,
                    version_id,
                    anchor,
                    author: &who,
                    visibility: held,
                    body: &text,
                    now,
                },
            ) {
                Ok(t) => Ok(Ok(t)),
                Err(CreateError::Db(e)) => Err(e),
                Err(other) => Ok(Err(other)),
            }
        })
        .await?;
    match made {
        Ok(thread) => {
            s.comment_written(&agent, held);
            Ok((StatusCode::CREATED, Json(thread)))
        }
        Err(CreateError::NoVersion) => Err(ApiError::NotFound),
        Err(CreateError::Anchor(e)) => Err(ApiError::BadRequest(e.to_string())),
        Err(CreateError::Db(_)) => unreachable!("database errors leave through run"),
    }
}

async fn agent_reply(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(tid): Path<String>,
    body: Result<Json<AgentCommentBody>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<Thread>), ApiError> {
    let agent = s.agent(&headers).await?;
    let b = json_body(body)?;
    let text = clean_body(&b.body)?;
    let held = visibility(b.held);
    let comment_id = margin_core::new_id(&*s.rng);
    let now = s.clock.now_unix();
    let who = agent.clone();
    let thread = s
        .run(move |conn| {
            let Some(id) = threads::collection_of(conn, &tid)? else {
                return Ok(None);
            };
            threads::add_comment(conn, &id, &tid, &comment_id, &who, held, &text, now)
        })
        .await?
        .ok_or(ApiError::NotFound)?;
    s.comment_written(&agent, held);
    Ok((StatusCode::CREATED, Json(thread)))
}

async fn agent_thread(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(tid): Path<String>,
) -> Result<Json<Thread>, ApiError> {
    s.agent(&headers).await?;
    s.run(move |conn| match threads::collection_of(conn, &tid)? {
        Some(id) => threads::get(conn, Scope::Agent, &id, &tid),
        None => Ok(None),
    })
    .await?
    .map(Json)
    .ok_or(ApiError::NotFound)
}

async fn agent_list_threads(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ThreadListing>, ApiError> {
    s.agent(&headers).await?;
    threads_listing(&s, Scope::Agent, id, q).await
}

async fn agent_edit_comment(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(cid): Path<String>,
    body: Result<Json<BodyOnly>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Thread>, ApiError> {
    let agent = s.agent(&headers).await?;
    let text = clean_body(&json_body(body)?.body)?;
    let now = s.clock.now_unix();
    match s
        .run(move |conn| threads::edit_comment(conn, None, &agent, &cid, &text, now))
        .await?
    {
        Change::NotFound => Err(ApiError::NotFound),
        Change::NotYours => Err(ApiError::Forbidden),
        Change::Done(thread) => Ok(Json(thread)),
    }
}

async fn agent_delete_comment(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(cid): Path<String>,
) -> Result<StatusCode, ApiError> {
    let agent = s.agent(&headers).await?;
    let now = s.clock.now_unix();
    match s
        .run(move |conn| threads::delete_comment(conn, None, &agent, &cid, now))
        .await?
    {
        Change::NotFound => Err(ApiError::NotFound),
        Change::NotYours => Err(ApiError::Forbidden),
        Change::Done(_) => Ok(StatusCode::NO_CONTENT),
    }
}

/// The reader taps Publish: every held comment of the collection appears beside his.
async fn publish(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let now = s.clock.now_unix();
    match s.run(move |conn| threads::publish(conn, &id, now)).await? {
        Published::NotFound => Err(ApiError::NotFound),
        Published::Closed => Err(ApiError::Conflict(
            "a closed collection cannot be published".into(),
        )),
        Published::Done(released) => Ok(Json(
            serde_json::json!({ "state": "published", "released": released }),
        )),
    }
}

#[derive(Deserialize)]
struct FeedQuery {
    after: Option<i64>,
    wait: Option<u64>,
}

impl FeedQuery {
    fn after(&self) -> i64 {
        self.after.unwrap_or(0).max(0)
    }

    fn wait(&self) -> Duration {
        feed::clamp_wait(self.wait.unwrap_or(0))
    }
}

#[derive(Serialize)]
struct AgentFeed {
    changes: Vec<changes::Row>,
    last_seq: i64,
}

/// Every change after `after`; held open up to `wait` seconds (at most 30) when there is none.
async fn agent_changes(
    State(s): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<FeedQuery>,
) -> Result<Json<AgentFeed>, ApiError> {
    s.agent(&headers).await?;
    let after = q.after();
    let started = Instant::now();
    let batch = feed::long_poll(s.db.notifier(), q.wait(), || {
        let s = s.clone();
        async move {
            let b = s
                .run(move |conn| changes::read(conn, View::Agent, after))
                .await?;
            Ok::<_, ApiError>(if b.rows.is_empty() {
                Probe::Empty(b)
            } else {
                Probe::Ready(b)
            })
        }
    })
    .await?;
    s.feed_answered(started, &batch.rows);
    Ok(Json(AgentFeed {
        changes: batch.rows,
        last_seq: batch.last_seq,
    }))
}

#[derive(Serialize)]
struct PageThread {
    id: String,
    version_id: i64,
    last_line: u32,
    /// The rendered `thread.html`, closed.
    html: String,
}

#[derive(Serialize)]
struct VersionMarks {
    version_id: i64,
    marks: Vec<Mark>,
}

#[derive(Serialize)]
struct PageFeed {
    last_seq: i64,
    /// A version arrived: the page shows the newest, so it reloads.
    reload: bool,
    state: String,
    /// Threads to put on the page, each replacing the one it has there or going in after its
    /// last line.
    threads: Vec<PageThread>,
    /// Threads the reader can no longer see.
    gone: Vec<String>,
    /// The marks of each version, when anything changed.
    marks: Vec<VersionMarks>,
}

/// What one look at a collection's feed found, before the threads are rendered.
struct PageLook {
    last_seq: i64,
    state: String,
    changed: bool,
    reload: bool,
    threads: Vec<Thread>,
    gone: Vec<String>,
    marks: Vec<(i64, Vec<Mark>)>,
    rows: Vec<changes::Row>,
}

fn look(
    conn: &mut rusqlite::Connection,
    id: &str,
    after: i64,
) -> rusqlite::Result<Option<PageLook>> {
    let state: Option<String> = rusqlite::OptionalExtension::optional(conn.query_row(
        "SELECT state FROM collections WHERE id = ?1",
        [id],
        |r| r.get(0),
    ))?;
    let Some(state) = state else { return Ok(None) };
    let batch = changes::read(conn, View::Page(id), after)?;
    let mut look = PageLook {
        last_seq: batch.last_seq,
        state,
        changed: !batch.rows.is_empty(),
        reload: batch.rows.iter().any(|r| r.kind == "version_created"),
        threads: Vec::new(),
        gone: Vec::new(),
        marks: Vec::new(),
        rows: Vec::new(),
    };
    if look.changed {
        let mut seen = std::collections::HashSet::new();
        let touched: Vec<&str> = batch
            .rows
            .iter()
            .filter_map(|r| r.thread_id.as_deref())
            .filter(|t| seen.insert(*t))
            .collect();
        look.threads = if batch.rows.iter().any(|r| r.kind == "published") {
            threads::all(conn, Scope::Reader, id)?
        } else {
            let mut found = Vec::new();
            for t in &touched {
                found.extend(threads::get(conn, Scope::Reader, id, t)?);
            }
            found
        };
        look.gone = touched
            .iter()
            .filter(|t| !look.threads.iter().any(|f| f.id == **t))
            .map(|t| t.to_string())
            .collect();
        look.marks = threads::collection_marks(conn, id)?;
    }
    look.rows = batch.rows;
    Ok(Some(look))
}

/// The reader page's feed of its own collection: no held comment, nor any change to one.
async fn page_changes(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FeedQuery>,
) -> Result<Json<PageFeed>, ApiError> {
    let after = q.after();
    let started = Instant::now();
    let found = feed::long_poll(s.db.notifier(), q.wait(), || {
        let s = s.clone();
        let id = id.clone();
        async move {
            let look = s.run(move |conn| look(conn, &id, after)).await?;
            Ok::<_, ApiError>(match look {
                None => return Err(ApiError::NotFound),
                Some(l) if l.changed => Probe::Ready(l),
                Some(l) => Probe::Empty(l),
            })
        }
    })
    .await?;
    s.feed_answered(started, &found.rows);
    let threads = found
        .threads
        .iter()
        .map(|t| {
            let html = crate::page::ThreadFragment {
                thread: crate::page::ThreadView::new(t, false),
            }
            .render()
            .map_err(|e| ApiError::Internal(e.to_string()))?;
            Ok(PageThread {
                id: t.id.clone(),
                version_id: t.version_id,
                last_line: t.last_line,
                html,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(PageFeed {
        last_seq: found.last_seq,
        reload: found.reload,
        state: found.state,
        threads,
        gone: found.gone,
        marks: found
            .marks
            .into_iter()
            .map(|(version_id, marks)| VersionMarks { version_id, marks })
            .collect(),
    }))
}

#[derive(Deserialize)]
struct VersionBody {
    text: String,
}

#[derive(Serialize)]
struct Listed {
    items: Vec<versions::Version>,
    next: Option<String>,
}

/// POST the next version of a document; its threads are carried onto it.
async fn create_version(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    body: Result<Json<VersionBody>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<versions::Version>), ApiError> {
    s.agent(&headers).await?;
    let b = json_body(body)?;
    let now = s.clock.now_unix();
    let added = s
        .run(move |conn| versions::add(conn, &doc_id, &b.text, now))
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok((StatusCode::CREATED, Json(added.version)))
}

async fn list_versions(
    State(s): State<AppState>,
    Path(doc_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Listed>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_PAGE);
    if limit == 0 || limit > MAX_PAGE {
        return Err(ApiError::BadRequest(format!(
            "limit must be 1..={MAX_PAGE}"
        )));
    }
    let after = match q.after {
        None => 0,
        Some(a) => a
            .parse::<i64>()
            .map_err(|_| ApiError::BadRequest("after is a version number".into()))?,
    };
    let mut items = s
        .run(move |conn| versions::list(conn, &doc_id, limit + 1, after))
        .await?
        .ok_or(ApiError::NotFound)?;
    let next = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|v| v.number.to_string())
    } else {
        None
    };
    Ok(Json(Listed { items, next }))
}

async fn read_version(
    State(s): State<AppState>,
    Path((doc_id, n)): Path<(String, i64)>,
) -> Result<Json<versions::Version>, ApiError> {
    let found = s.run(move |conn| versions::get(conn, &doc_id, n)).await?;
    found.map(Json).ok_or(ApiError::NotFound)
}

/// Only the latest version, and never version 1; its threads go back to their earlier anchors.
async fn delete_version(
    State(s): State<AppState>,
    headers: HeaderMap,
    Path((doc_id, n)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError> {
    s.agent(&headers).await?;
    let now = s.clock.now_unix();
    let (outcome, _) = s
        .run(move |conn| versions::delete_latest(conn, &doc_id, n, now))
        .await?;
    match outcome {
        versions::Deleted::Gone => Ok(StatusCode::NO_CONTENT),
        versions::Deleted::NoSuchVersion => Err(ApiError::NotFound),
        versions::Deleted::NotAllowed => Err(ApiError::Conflict(
            "only the latest version can be deleted, and never version 1".into(),
        )),
        versions::Deleted::HasNewThreads => Err(ApiError::Conflict(
            "a thread was started on this version; it has no earlier anchor to return to".into(),
        )),
    }
}
