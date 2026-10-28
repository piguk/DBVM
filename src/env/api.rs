//! The HTTP API (`/v1`). Every route authenticates with `Authorization: Bearer <key>` and calls the
//! service on a blocking thread: sandboxes and SQLite block, and a long command must not stall the server.
//!
//! Errors are JSON `{"error": "..."}` with 401 (no or unknown key), 404 (not found, or another tenant's),
//! 400 (malformed request), 429 (quota) or 500.

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;

use super::control::Tenant;
use super::service::{CreateSession, EnvError, Service, Variant};
use super::task::{Action, TaskSpec};

impl IntoResponse for EnvError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            EnvError::Unauthorized => (StatusCode::UNAUTHORIZED, "missing or unknown API key".to_string()),
            EnvError::NotFound(w) => (StatusCode::NOT_FOUND, format!("{} not found", w)),
            EnvError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            EnvError::Quota(m) => (StatusCode::TOO_MANY_REQUESTS, m),
            EnvError::Internal(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{:#}", e)),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

type ApiResult = Result<Response, EnvError>;

/// Run `f` with the caller's tenant on a blocking thread.
async fn call<T: serde::Serialize + Send + 'static>(svc: Arc<Service>, headers: HeaderMap,
        f: impl FnOnce(&Service, &Tenant) -> Result<T, EnvError> + Send + 'static) -> ApiResult {
    let key = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::to_string);
    let out = tokio::task::spawn_blocking(move || {
        let t = svc.authenticate(key.as_deref().ok_or(EnvError::Unauthorized)?)?;
        f(&svc, &t)
    }).await.map_err(|e| EnvError::Internal(anyhow::anyhow!("worker failed: {}", e)))??;
    Ok(Json(out).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Variants { variants: Vec<Variant> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RolloutsReq { task_id: String, variants: Vec<Variant> }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FreezeReq { name: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchItem { session_id: String, action: Action }

pub fn router(svc: Arc<Service>) -> Router {
    Router::new()
        .route("/v1/templates", get(|State(s): State<Arc<Service>>, h: HeaderMap| async move {
            call(s, h, |s, t| s.list_templates(t)).await }))
        .route("/v1/templates/{id}", delete(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.delete_template(t, &id).map(|_| serde_json::json!({}))).await }))
        .route("/v1/tasks", post(|State(s): State<Arc<Service>>, h: HeaderMap, Json(spec): Json<TaskSpec>| async move {
            call(s, h, move |s, t| s.create_task(t, spec).map(|id| serde_json::json!({ "task_id": id }))).await }))
        .route("/v1/tasks/{id}", get(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.task(t, &id)).await }))
        .route("/v1/sessions", post(|State(s): State<Arc<Service>>, h: HeaderMap, Json(req): Json<CreateSession>| async move {
            call(s, h, move |s, t| s.create_session(t, req).map(|id| serde_json::json!({ "session_id": id }))).await }))
        .route("/v1/sessions/{id}", delete(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.close(t, &id, "closed by the client").map(|_| serde_json::json!({}))).await }))
        .route("/v1/sessions/{id}/actions", post(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>, Json(a): Json<Action>| async move {
            call(s, h, move |s, t| s.act(t, &id, a)).await }))
        .route("/v1/sessions/{id}/fork", post(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>, Json(v): Json<Variants>| async move {
            call(s, h, move |s, t| s.fork(t, &id, v.variants).map(|ids| serde_json::json!({ "session_ids": ids }))).await }))
        .route("/v1/sessions/{id}/freeze", post(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>, Json(f): Json<FreezeReq>| async move {
            call(s, h, move |s, t| s.freeze(t, &id, &f.name)).await }))
        .route("/v1/sessions/{id}/judge", post(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.judge(t, &id)).await }))
        .route("/v1/sessions/{id}/trajectory", get(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            let key = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::to_string);
            let out = tokio::task::spawn_blocking(move || {
                let t = s.authenticate(key.as_deref().ok_or(EnvError::Unauthorized)?)?;
                s.trajectory(&t, &id)
            }).await.map_err(|e| EnvError::Internal(anyhow::anyhow!("worker failed: {}", e)))??;
            Ok::<Response, EnvError>(([("content-type", "application/x-ndjson")], out).into_response()) }))
        .route("/v1/rollouts", post(|State(s): State<Arc<Service>>, h: HeaderMap, Json(r): Json<RolloutsReq>| async move {
            call(s, h, move |s, t| s.rollouts(t, &r.task_id, r.variants).map(|ids| serde_json::json!({ "session_ids": ids }))).await }))
        .route("/v1/actions", post(batch))
        .route("/v1/usage", get(|State(s): State<Arc<Service>>, h: HeaderMap| async move {
            call(s, h, |s, t| s.usage(t)).await }))
        // write_file carries file contents
        .layer(DefaultBodyLimit::max(64 << 20))
        .with_state(svc)
}

/// One action in each of several sessions, run concurrently. Each item answers with its response or its
/// error, in order.
async fn batch(State(s): State<Arc<Service>>, h: HeaderMap, Json(items): Json<Vec<BatchItem>>) -> ApiResult {
    let key = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::to_string)
        .ok_or(EnvError::Unauthorized)?;
    let s2 = s.clone();
    let k2 = key.clone();
    let t = tokio::task::spawn_blocking(move || s2.authenticate(&k2)).await.map_err(|e| EnvError::Internal(anyhow::anyhow!("{}", e)))??;
    let mut handles = Vec::new();
    for it in items {
        let (s, t) = (s.clone(), t.clone());
        handles.push(tokio::task::spawn_blocking(move || s.act(&t, &it.session_id, it.action)));
    }
    let mut out = Vec::new();
    for h in handles {
        out.push(match h.await {
            Ok(Ok(r)) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            Ok(Err(e)) => serde_json::json!({ "error": format!("{:?}", e) }),
            Err(e) => serde_json::json!({ "error": e.to_string() }),
        });
    }
    Ok(Json(out).into_response())
}

/// Serve until the process is stopped; closes expired sessions every few seconds.
pub async fn serve(svc: Arc<Service>, listen: &str) -> anyhow::Result<()> {
    let reaper = svc.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let r = reaper.clone();
            let _ = tokio::task::spawn_blocking(move || r.reap()).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, router(svc)).await?;
    Ok(())
}
