//! HTTP API. Every execution is asynchronous: persist, enqueue, return 202;
//! the verdict arrives by polling `GET /v1/submissions/{id}` or over SSE.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use codexec_judge::{Comparer, TestCase};
use futures_util::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::runs::{QueueFull, RunJob};
use crate::store::{NewSubmission, Status, Submission};
use crate::AppState;

/// No authentication yet: every request acts as this user.
const LOCAL_USER: &str = "local";
const DEFAULT_RUN_TIME_LIMIT_MS: u64 = 1000;
const DEFAULT_RUN_MEMORY_KB: u64 = 256 * 1024;
const MAX_STDIN_BYTES: usize = 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    // JSON escaping can inflate a source file; leave head-room above max_source_bytes.
    let body_limit = state.config.max_source_bytes * 2 + MAX_STDIN_BYTES * 2 + 16 * 1024;
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/languages", get(list_languages))
        .route("/v1/problems", get(list_problems))
        .route("/v1/problems/{slug}", get(get_problem))
        .route("/v1/submissions", post(create_submission).get(list_submissions))
        .route("/v1/submissions/{id}", get(get_submission))
        .route("/v1/submissions/{id}/source", get(get_submission_source))
        .route("/v1/submissions/{id}/events", get(submission_events))
        .route("/v1/runs", post(create_run))
        .route("/v1/runs/{id}", get(get_run))
        .layer(RequestBodyLimitLayer::new(body_limit))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

// ---------- errors ----------

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(what: &str) -> Self {
        Self(StatusCode::NOT_FOUND, format!("{what} not found"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!(error = %e, "internal error");
        Self(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------- meta ----------

async fn healthz(State(state): State<Arc<AppState>>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({
        "status": "ok",
        "queued_submissions": state.store.queue_depth().await?,
        "queued_runs": state.runs.queue_depth(),
        "problems": state.problems.len(),
        "languages": state.judge.languages().len(),
    })))
}

async fn list_languages(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({ "languages": state.judge.languages().infos() }))
}

// ---------- problems ----------

#[derive(Serialize)]
struct ProblemSummary<'a> {
    slug: &'a str,
    title: &'a str,
    difficulty: &'a str,
    tags: &'a [String],
}

#[derive(Serialize)]
struct Sample {
    name: String,
    input: String,
    output: String,
}

async fn list_problems(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let problems: Vec<_> = state
        .problems
        .iter()
        .map(|p| ProblemSummary { slug: &p.slug, title: &p.title, difficulty: &p.difficulty, tags: &p.tags })
        .collect();
    Json(json!({ "problems": problems }))
}

async fn get_problem(
    State(state): State<Arc<AppState>>,
    Path(slug): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let p = state.problems.get(&slug).ok_or_else(|| ApiError::not_found("problem"))?;
    let samples: Vec<Sample> = p
        .samples()
        .into_iter()
        .map(|t| Sample {
            name: t.name,
            input: String::from_utf8_lossy(&t.input).into_owned(),
            output: String::from_utf8_lossy(t.expected.as_deref().unwrap_or_default()).into_owned(),
        })
        .collect();
    Ok(Json(json!({
        "slug": p.slug,
        "title": p.title,
        "difficulty": p.difficulty,
        "tags": p.tags,
        "time_limit_ms": p.time_limit_ms,
        "memory_limit_mb": p.memory_limit_kb / 1024,
        "statement": p.statement,
        "samples": samples,
        "tests_total": p.tests.len(),
        "version": p.version,
    })))
}

// ---------- submissions ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSubmission {
    problem: String,
    language: String,
    source: String,
}

fn check_source(state: &AppState, language: &str, source: &str) -> ApiResult<()> {
    if state.judge.languages().get(language).is_none() {
        let known: Vec<&str> = state.judge.languages().ids().collect();
        return Err(ApiError::bad_request(format!(
            "unknown language {language:?}; available: {}",
            known.join(", ")
        )));
    }
    if source.trim().is_empty() {
        return Err(ApiError::bad_request("source is empty"));
    }
    if source.len() > state.config.max_source_bytes {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("source exceeds {} bytes", state.config.max_source_bytes),
        ));
    }
    Ok(())
}

async fn create_submission(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<CreateSubmission>,
) -> ApiResult<Response> {
    let problem = state.problems.get(&body.problem).ok_or_else(|| ApiError::not_found("problem"))?;
    check_source(&state, &body.language, &body.source)?;

    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty() && k.len() <= 128)
        .map(|k| format!("{LOCAL_USER}:{k}"));

    // Persisted before the 202: from here on the submission cannot be lost.
    let (sub, created) = state
        .store
        .insert(NewSubmission {
            user_id: LOCAL_USER.into(),
            problem: problem.slug.clone(),
            problem_version: problem.version.clone(),
            language: body.language,
            source: body.source,
            tests_total: problem.tests.len() as u64,
            idempotency_key,
        })
        .await?;
    if created {
        state.wake.notify_one();
    }

    let location = format!("/v1/submissions/{}", sub.id);
    Ok((StatusCode::ACCEPTED, [(header::LOCATION, location)], Json(sub)).into_response())
}

#[derive(Deserialize)]
struct ListQuery {
    problem: Option<String>,
    limit: Option<u32>,
}

async fn list_submissions(
    State(state): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let submissions = state.store.list(q.problem, limit).await?;
    Ok(Json(json!({ "submissions": submissions })))
}

async fn get_submission(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Json<Submission>> {
    let sub = state.store.get(&id).await?.ok_or_else(|| ApiError::not_found("submission"))?;
    Ok(Json(sub))
}

async fn get_submission_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let source = state.store.get_source(&id).await?.ok_or_else(|| ApiError::not_found("submission"))?;
    // Always plain text: user-supplied content is never served as HTML.
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], source).into_response())
}

/// SSE: the current state immediately, then every change, closing after DONE.
async fn submission_events(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    // Subscribe before reading, so no transition can fall between the two.
    let rx = state.hub.subscribe(&id);
    let current = state.store.get(&id).await?.ok_or_else(|| ApiError::not_found("submission"))?;

    // unfold rather than scan/take_while: the stream must end right after the
    // DONE item, not when the next item (which never comes) would arrive.
    let events = stream::unfold((Some(current), rx, false), |(first, mut rx, finished)| async move {
        if finished {
            return None;
        }
        let sub = match first {
            Some(sub) => sub,
            None => loop {
                match rx.recv().await {
                    Ok(sub) => break sub,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return None,
                }
            },
        };
        let done = sub.status == Status::Done;
        Some((sub, (None, rx, done)))
    })
    .map(|sub| {
        let name = if sub.status == Status::Done { "verdict" } else { "progress" };
        Ok(Event::default().event(name).json_data(&sub).unwrap_or_default())
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

// ---------- runs ----------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRun {
    language: String,
    source: String,
    /// Custom input. When absent and `problem` is set, the problem's samples are used.
    stdin: Option<String>,
    /// Supplies limits and the comparer.
    problem: Option<String>,
}

async fn create_run(State(state): State<Arc<AppState>>, Json(body): Json<CreateRun>) -> ApiResult<Response> {
    check_source(&state, &body.language, &body.source)?;
    if body.stdin.as_ref().is_some_and(|s| s.len() > MAX_STDIN_BYTES) {
        return Err(ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("stdin exceeds {MAX_STDIN_BYTES} bytes"),
        ));
    }
    let problem = match &body.problem {
        Some(slug) => Some(state.problems.get(slug).ok_or_else(|| ApiError::not_found("problem"))?),
        None => None,
    };

    let tests = match (body.stdin, &problem) {
        (Some(stdin), _) => {
            vec![TestCase { name: "custom".into(), input: stdin.into_bytes(), expected: None }]
        }
        (None, Some(p)) if !p.sample_names.is_empty() => p.samples(),
        (None, _) => vec![TestCase { name: "custom".into(), input: Vec::new(), expected: None }],
    };

    let job = RunJob {
        id: String::new(),
        language: body.language,
        source: body.source,
        tests,
        time_limit_ms: problem.as_ref().map_or(DEFAULT_RUN_TIME_LIMIT_MS, |p| p.time_limit_ms),
        memory_limit_kb: problem.as_ref().map_or(DEFAULT_RUN_MEMORY_KB, |p| p.memory_limit_kb),
        comparer: problem.as_ref().map_or(Comparer::Lines, |p| p.comparer),
    };
    match state.runs.submit(job) {
        Ok(record) => {
            state.wake.notify_one();
            let location = format!("/v1/runs/{}", record.id);
            Ok((StatusCode::ACCEPTED, [(header::LOCATION, location)], Json(record)).into_response())
        }
        Err(QueueFull) => {
            Err(ApiError(StatusCode::SERVICE_UNAVAILABLE, "run queue is full, retry shortly".into()))
        }
    }
}

async fn get_run(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Response> {
    let record = state.runs.get(&id).ok_or_else(|| ApiError::not_found("run (results expire)"))?;
    Ok(Json(record).into_response())
}
