//! HTTP routes. Handlers only await: SQLite runs on the [`Db`] workers and
//! highlighting on the blocking pool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, MatchedPath, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use margin_core::{Clock, Rng};
use serde::{Deserialize, Serialize};
use tower_http::timeout::TimeoutLayer;

use crate::config::Config;
use crate::db::{Db, DbError};
use crate::highlight::Highlighter;
use crate::metrics::{Event, Metrics};
use crate::store::{self, NewCollection, NewDocument, Patch};

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
    Router::new()
        .route("/api/collections", get(list).post(create))
        .route(
            "/api/collections/{id}",
            get(read).patch(update).delete(remove),
        )
        .route("/c/{id}", get(reader))
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
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::GATEWAY_TIMEOUT,
            REQUEST_DEADLINE,
        ))
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
        Some(c) => {
            let (at, id) = c
                .split_once('.')
                .ok_or_else(|| ApiError::BadRequest("after is not a cursor".into()))?;
            let at: i64 = at
                .parse()
                .map_err(|_| ApiError::BadRequest("after is not a cursor".into()))?;
            Some((at, id.to_string()))
        }
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
    let collection = s
        .run(move |conn| store::get(conn, &id))
        .await?
        .ok_or(ApiError::NotFound)?;
    let highlighter = s.highlighter.clone();
    let started = Instant::now();
    let html = tokio::task::spawn_blocking(move || crate::page::render(&collection, &highlighter))
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
