//! The HTTP API (`/v1`). Every route authenticates with `Authorization: Bearer <key>` and calls the
//! service on a blocking thread: sandboxes and SQLite block, and a long command must not stall the server.
//!
//! Errors are JSON `{"error": "..."}` with 401 (no or unknown key), 404 (not found, or another tenant's),
//! 400 (malformed request), 429 (quota) or 500.

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;

use super::control::Tenant;
use super::service::{CreateSession, EnvError, Service, Variant};
use super::task::{Action, TaskSpec};

impl EnvError {
    pub fn status(&self) -> StatusCode {
        match self {
            EnvError::Unauthorized => StatusCode::UNAUTHORIZED,
            EnvError::NotFound(_) => StatusCode::NOT_FOUND,
            EnvError::BadRequest(_) => StatusCode::BAD_REQUEST,
            EnvError::Quota(_) => StatusCode::TOO_MANY_REQUESTS,
            EnvError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            EnvError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl std::fmt::Display for EnvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvError::Unauthorized => write!(f, "missing or unknown API key"),
            EnvError::NotFound(w) => write!(f, "{} not found", w),
            EnvError::BadRequest(m) | EnvError::Quota(m) | EnvError::Unavailable(m) => write!(f, "{}", m),
            EnvError::Internal(e) => write!(f, "{:#}", e),
        }
    }
}

impl IntoResponse for EnvError {
    fn into_response(self) -> Response {
        (self.status(), Json(serde_json::json!({ "error": self.to_string() }))).into_response()
    }
}

type ApiResult = Result<Response, EnvError>;

/// The bearer token of a request.
pub fn bearer(h: &HeaderMap) -> Option<String> {
    h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(str::to_string)
}

/// The tenant a request acts for: the one its API key belongs to, or, from the coordinator of a cluster (the
/// cluster token), the one it names in `X-Selfdb-Tenant`.
fn tenant_of(svc: &Service, key: Option<&str>, tenant: Option<&str>) -> Result<Tenant, EnvError> {
    let key = key.ok_or(EnvError::Unauthorized)?;
    match tenant { Some(t) => svc.cluster_tenant(key, t), None => svc.authenticate(key) }
}

fn tenant_header(h: &HeaderMap) -> Option<String> { h.get("x-selfdb-tenant").and_then(|v| v.to_str().ok()).map(str::to_string) }

/// Run `f` with the caller's tenant on a blocking thread.
async fn call<T: serde::Serialize + Send + 'static>(svc: Arc<Service>, headers: HeaderMap,
        f: impl FnOnce(&Service, &Tenant) -> Result<T, EnvError> + Send + 'static) -> ApiResult {
    let (key, tenant) = (bearer(&headers), tenant_header(&headers));
    let out = tokio::task::spawn_blocking(move || {
        let t = tenant_of(&svc, key.as_deref(), tenant.as_deref())?;
        f(&svc, &t)
    }).await.map_err(|e| EnvError::Internal(anyhow::anyhow!("worker failed: {}", e)))??;
    Ok(Json(out).into_response())
}

/// Run `f` for the coordinator or another node of the cluster (they present the cluster token).
async fn internal<T: serde::Serialize + Send + 'static>(svc: Arc<Service>, headers: HeaderMap,
        f: impl FnOnce(&Service) -> Result<T, EnvError> + Send + 'static) -> ApiResult {
    let key = bearer(&headers);
    let out = tokio::task::spawn_blocking(move || {
        if !key.map(|k| svc.is_cluster_token(&k)).unwrap_or(false) { return Err(EnvError::Unauthorized); }
        f(&svc)
    }).await.map_err(|e| EnvError::Internal(anyhow::anyhow!("worker failed: {}", e)))??;
    Ok(Json(out).into_response())
}

/// Stream what `write` produces (on a blocking thread) as the response body, for the cluster's nodes.
async fn internal_stream(svc: Arc<Service>, headers: HeaderMap,
        write: impl FnOnce(&Service, &mut dyn std::io::Write) -> Result<(), EnvError> + Send + 'static) -> ApiResult {
    if !bearer(&headers).map(|k| svc.is_cluster_token(&k)).unwrap_or(false) { return Err(EnvError::Unauthorized); }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(16);
    tokio::task::spawn_blocking(move || {
        let mut w = ChannelWriter { tx: tx.clone() };
        if let Err(e) = write(&svc, &mut w) {
            // the receiver sees an error and the response ends short: the reader's digest check fails
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });
    let body = axum::body::Body::from_stream(futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx)));
    Ok(([("content-type", "application/octet-stream")], body).into_response())
}

struct ChannelWriter { tx: tokio::sync::mpsc::Sender<std::io::Result<bytes::Bytes>> }

impl std::io::Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tx.blocking_send(Ok(bytes::Bytes::copy_from_slice(buf))).map_err(|_| std::io::Error::other("the receiver is gone"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
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
struct PutTask { tenant_id: String, spec: TaskSpec }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fetch { from: String }

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
            call(s, h, move |s, t| s.task(t, &id)).await })
            .delete(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.delete_task(t, &id).map(|_| serde_json::json!({}))).await }))
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
            let (key, tenant) = (bearer(&h), tenant_header(&h));
            let out = tokio::task::spawn_blocking(move || {
                let t = tenant_of(&s, key.as_deref(), tenant.as_deref())?;
                s.trajectory(&t, &id)
            }).await.map_err(|e| EnvError::Internal(anyhow::anyhow!("worker failed: {}", e)))??;
            Ok::<Response, EnvError>(([("content-type", "application/x-ndjson")], out).into_response()) }))
        .route("/v1/sessions/{id}/checkpoints", post(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.checkpoint(t, &id)).await })
            .get(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.checkpoints(t, &id)).await }))
        .route("/v1/checkpoints/{id}", delete(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            call(s, h, move |s, t| s.delete_checkpoint(t, &id).map(|_| serde_json::json!({}))).await }))
        .route("/v1/checkpoints/{id}/fork", post(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>, Json(v): Json<Variants>| async move {
            call(s, h, move |s, t| s.fork_checkpoint(t, &id, v.variants).map(|ids| serde_json::json!({ "session_ids": ids }))).await }))
        .route("/v1/rollouts", post(|State(s): State<Arc<Service>>, h: HeaderMap, Json(r): Json<RolloutsReq>| async move {
            call(s, h, move |s, t| s.rollouts(t, &r.task_id, r.variants).map(|ids| serde_json::json!({ "session_ids": ids }))).await }))
        .route("/v1/actions", post(batch))
        // the cluster's own routes: the coordinator and the other nodes, with the cluster token
        .route("/internal/v1/status", get(|State(s): State<Arc<Service>>, h: HeaderMap| async move {
            internal(s, h, |s| s.status()).await }))
        .route("/internal/v1/tenants/{id}", put(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>, Json(t): Json<Tenant>| async move {
            internal(s, h, move |s| if t.id != id { Err(EnvError::BadRequest("tenant id mismatch".into())) } else { s.put_tenant(&t).map(|_| serde_json::json!({})) }).await }))
        .route("/internal/v1/tasks/{id}", put(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>, Json(t): Json<PutTask>| async move {
            internal(s, h, move |s| s.put_task(&id, &t.tenant_id, t.spec).map(|_| serde_json::json!({}))).await })
            .delete(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            internal(s, h, move |s| s.drop_task(&id).map(|_| serde_json::json!({}))).await }))
        .route("/internal/v1/templates/{id}", get(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            internal(s, h, move |s| s.template_meta(&id)).await })
            .delete(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            internal(s, h, move |s| s.drop_template(&id).map(|_| serde_json::json!({}))).await }))
        .route("/internal/v1/templates/{id}/fetch", post(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>, Json(f): Json<Fetch>| async move {
            internal(s, h, move |s| s.fetch_template(&id, &f.from)).await }))
        .route("/internal/v1/templates/{id}/db", get(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            internal_stream(s, h, move |s, w| {
                let p = s.template_db_path(&id)?;
                std::io::copy(&mut std::fs::File::open(p).map_err(anyhow::Error::from)?, w).map_err(anyhow::Error::from)?;
                Ok(())
            }).await }))
        .route("/internal/v1/layers/{id}", get(|State(s): State<Arc<Service>>, h: HeaderMap, Path(id): Path<String>| async move {
            internal_stream(s, h, move |s, w| { super::transfer::write_layer(&s.layer_path(&id)?, w)?; Ok(()) }).await }))
        .route("/v1/usage", get(|State(s): State<Arc<Service>>, h: HeaderMap| async move {
            call(s, h, |s, t| s.usage(t)).await }))
        // write_file carries file contents
        .layer(DefaultBodyLimit::max(64 << 20))
        .with_state(svc)
}

/// One action in each of several sessions, run concurrently. Each item answers with its response or its
/// error, in order.
async fn batch(State(s): State<Arc<Service>>, h: HeaderMap, Json(items): Json<Vec<BatchItem>>) -> ApiResult {
    let (key, tenant) = (bearer(&h), tenant_header(&h));
    let s2 = s.clone();
    let t = tokio::task::spawn_blocking(move || tenant_of(&s2, key.as_deref(), tenant.as_deref())).await
        .map_err(|e| EnvError::Internal(anyhow::anyhow!("{}", e)))??;
    let mut handles = Vec::new();
    for it in items {
        let (s, t) = (s.clone(), t.clone());
        handles.push(tokio::task::spawn_blocking(move || s.act(&t, &it.session_id, it.action)));
    }
    let mut out = Vec::new();
    for h in handles {
        out.push(match h.await {
            Ok(Ok(r)) => serde_json::to_value(r).unwrap_or(serde_json::Value::Null),
            Ok(Err(e)) => serde_json::json!({ "error": e.to_string() }),
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
