//! HTTP + SSE API for operator user interfaces.
//!
//! The web console and the terminal UI are pure clients of this API; the control plane stays one
//! process. Every route maps onto a runner or store operation that already exists, so the API adds
//! no semantics of its own — it cannot approve an action the matrix denied, and it records nothing
//! the CLI would not. It binds to localhost by default; an optional bearer token protects it when
//! an operator chooses to bind wider.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::domain::{HumanReport, IssuePriority, SnapshotCause};
use crate::error::AgentError;
use crate::ports::StateStore;
use crate::runner::SliceRunner;

/// Shared state behind every route.
pub struct ApiState {
    /// The wired control plane.
    pub runner: Arc<SliceRunner>,
    /// Effective configuration, served redacted.
    pub config: AppConfig,
    started: Instant,
}

impl ApiState {
    /// Creates the API state over a wired runner.
    pub fn new(runner: Arc<SliceRunner>, config: AppConfig) -> Self {
        Self {
            runner,
            config,
            started: Instant::now(),
        }
    }
}

/// JSON error body with the HTTP status the failure maps to.
struct ApiError(StatusCode, String);

impl From<AgentError> for ApiError {
    fn from(error: AgentError) -> Self {
        let status = match &error {
            AgentError::NotFound { .. } => StatusCode::NOT_FOUND,
            AgentError::InvalidInput(_) | AgentError::InvalidTransition { .. } => {
                StatusCode::BAD_REQUEST
            }
            AgentError::SchedulerFrozen { .. } | AgentError::Duplicate { .. } => {
                StatusCode::CONFLICT
            }
            AgentError::MissingDependency { .. } => StatusCode::NOT_IMPLEMENTED,
            AgentError::Serialization(_) | AgentError::Io { .. } => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        Self(status, error.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Builds the router with CORS (for a separately served Vite dev server) and optional auth.
pub fn router(state: Arc<ApiState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);
    Router::new()
        .route("/api/status", get(status))
        .route("/api/config", get(config))
        .route("/api/topology", get(topology))
        .route("/api/snapshots", post(capture_snapshot))
        .route("/api/snapshots/latest", get(latest_snapshot))
        .route("/api/issues", get(issues))
        .route("/api/jobs", get(jobs))
        .route("/api/reports", post(report))
        .route("/api/actions", get(actions))
        .route("/api/actions/{id}/approve", post(approve))
        .route("/api/actions/{id}/reject", post(reject))
        .route("/api/events", get(events))
        .route("/api/events/stream", get(events_stream))
        .route("/api/artifacts/{id}", get(artifact))
        .route("/api/artifacts/{id}/body", get(artifact_body))
        .route("/api/scheduler/{transition}", post(scheduler_transition))
        .layer(middleware::from_fn_with_state(state.clone(), require_token))
        .layer(cors)
        .with_state(state)
}

/// Serves the API until the process ends.
pub async fn serve(state: Arc<ApiState>, bind: &str) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Enforces the configured bearer token; the SSE route may pass it as `?token=` because browsers
/// cannot set headers on `EventSource`.
async fn require_token(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let expected = &state.config.api.token;
    if expected.is_empty() {
        return next.run(request).await;
    }
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(ToString::to_string)
        .or(query.token);
    if presented.as_deref() == Some(expected.as_str()) {
        next.run(request).await
    } else {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token".into(),
        )
        .into_response()
    }
}

#[derive(Debug, Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn status(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    let store = state.runner.store();
    let actions = store.list_action_runs().await?;
    let waiting = actions
        .iter()
        .filter(|a| a.status == crate::domain::ActionStatus::WaitingForApproval)
        .count();
    Ok(Json(json!({
        "mode": state.runner.scheduler().mode().await,
        "team_backend": state.runner.team_label(),
        "dry_run": state.runner.dry_run(),
        "deployment": state.runner.topology().deployment,
        "uptime_secs": state.started.elapsed().as_secs(),
        "counts": {
            "issues": store.list_issues().await?.len(),
            "jobs": store.list_jobs().await?.len(),
            "actions": actions.len(),
            "actions_waiting": waiting,
            "events": store.list_events().await?.len(),
        },
    })))
}

async fn config(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    Ok(Json(state.config.effective_json()?))
}

async fn topology(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(state.runner.topology())?))
}

async fn capture_snapshot(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    let snapshot = state.runner.capture(SnapshotCause::Manual).await?;
    Ok(Json(serde_json::to_value(snapshot)?))
}

async fn latest_snapshot(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    let snapshots = state.runner.store().list_snapshots().await?;
    let latest = snapshots
        .into_iter()
        .next_back()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "no Snapshot captured yet".into()))?;
    Ok(Json(serde_json::to_value(latest)?))
}

async fn issues(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(
        state.runner.store().list_issues().await?,
    )?))
}

async fn jobs(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(
        state.runner.store().list_jobs().await?,
    )?))
}

#[derive(Debug, Deserialize)]
struct ReportRequest {
    title: String,
    description: String,
    #[serde(default)]
    reporter: Option<String>,
    #[serde(default)]
    priority: Option<IssuePriority>,
}

async fn report(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<ReportRequest>,
) -> ApiResult<Value> {
    if request.title.trim().is_empty() || request.description.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "title and description are required".into(),
        ));
    }
    let mut report = HumanReport::new(
        request.reporter.unwrap_or_else(|| "console".to_string()),
        request.title,
        request.description,
    );
    report.priority = request.priority;
    let (issue, job) = state.runner.handle_report(report).await?;
    let actions = state.runner.run_proposals(&job).await?;
    Ok(Json(
        json!({ "issue": issue, "job": job, "actions": actions }),
    ))
}

async fn actions(State(state): State<Arc<ApiState>>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(
        state.runner.list_actions().await?,
    )?))
}

async fn approve(State(state): State<Arc<ApiState>>, Path(id): Path<Uuid>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(
        state.runner.approve_action(id).await?,
    )?))
}

async fn reject(State(state): State<Arc<ApiState>>, Path(id): Path<Uuid>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(
        state.runner.reject_action(id).await?,
    )?))
}

#[derive(Debug, Deserialize)]
struct EventsQuery {
    /// Return only events with a sequence greater than this.
    #[serde(default)]
    after: u64,
    /// Return at most this many of the newest matching events.
    limit: Option<usize>,
}

async fn events(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<EventsQuery>,
) -> ApiResult<Value> {
    let events = state.runner.store().list_events().await?;
    let mut selected: Vec<_> = events
        .into_iter()
        .filter(|event| event.sequence > query.after)
        .collect();
    if let Some(limit) = query.limit {
        let skip = selected.len().saturating_sub(limit);
        selected.drain(..skip);
    }
    Ok(Json(serde_json::to_value(selected)?))
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    /// Start after this sequence; omitted means "only events from now on".
    after: Option<u64>,
}

/// Streams new events as they are appended, polling the store twice a second.
async fn events_stream(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<StreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let store = state.runner.store();
    let start = match query.after {
        Some(after) => after,
        None => store
            .list_events()
            .await?
            .last()
            .map_or(0, |event| event.sequence),
    };
    let batches = stream::unfold((store, start), |(store, last)| async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let Ok(events) = store.list_events().await else {
                continue;
            };
            let fresh: Vec<_> = events
                .into_iter()
                .filter(|event| event.sequence > last)
                .collect();
            if let Some(newest) = fresh.last() {
                let next = newest.sequence;
                return Some((fresh, (store, next)));
            }
        }
    });
    let events = batches.flat_map(|batch| {
        stream::iter(batch.into_iter().map(|record| {
            let data = serde_json::to_string(&record).unwrap_or_default();
            Ok(Event::default().event("log").data(data))
        }))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

async fn artifact(State(state): State<Arc<ApiState>>, Path(id): Path<Uuid>) -> ApiResult<Value> {
    Ok(Json(serde_json::to_value(
        state.runner.store().get_artifact(id).await?,
    )?))
}

async fn artifact_body(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let artifact = state.runner.store().get_artifact(id).await?;
    let bytes = state.runner.artifacts().read_verified(&artifact)?;
    let content_type = if serde_json::from_slice::<Value>(&bytes).is_ok() {
        "application/json"
    } else {
        "application/octet-stream"
    };
    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
}

async fn scheduler_transition(
    State(state): State<Arc<ApiState>>,
    Path(transition): Path<String>,
) -> ApiResult<Value> {
    let scheduler = state.runner.scheduler();
    match transition.as_str() {
        "freeze-dispatch" => scheduler.freeze_dispatch().await?,
        "freeze-all" => scheduler.freeze_all().await?,
        "resume" => scheduler.resume().await?,
        other => {
            return Err(ApiError(
                StatusCode::NOT_FOUND,
                format!("unknown transition `{other}`; use freeze-dispatch, freeze-all, or resume"),
            ));
        }
    }
    Ok(Json(json!({ "mode": scheduler.mode().await })))
}
