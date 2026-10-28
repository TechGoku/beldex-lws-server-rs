//! The admin and operations API. Served on its own listener, which should be
//! bound to loopback or a private network. Every route except `/health`,
//! `/ready` and `/metrics` requires `Authorization: Bearer <token>`; the token
//! is compared in constant time and never logged.
//!
//! Bodies are JSON. For compatibility with the reference server's admin
//! calls, parameters may be wrapped as `{"params": {...}}`.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use lws_core::{account_flags, now_secs, AccountStatus, RequestKind, ViewKey};
use lws_store::{CreateOutcome, NewAccount};
use serde::Deserialize;
use serde_json::{json, Value as Json};
use subtle::ConstantTimeEq;
use tracing::info;

use crate::error::{ApiError, ApiResult};
use crate::wire::hex32;
use crate::{json_bytes, json_response, AppState};

pub struct AdminState {
    pub app: Arc<AppState>,
    /// SHA-free constant-time comparison works on the raw token bytes.
    pub token: Option<Vec<u8>>,
}

async fn authorize(State(state): State<Arc<AdminState>>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::as_bytes);
    let ok = match (&state.token, presented) {
        (Some(expected), Some(given)) => expected.len() == given.len() && bool::from(expected.as_slice().ct_eq(given)),
        _ => false,
    };
    if !ok {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    next.run(req).await
}

fn params<T: for<'de> Deserialize<'de>>(body: &Bytes) -> ApiResult<T> {
    let mut v: Json = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(body).map_err(|e| ApiError::BadRequest(format!("invalid JSON: {e}")))?
    };
    if let Some(inner) = v.get_mut("params") {
        v = inner.take();
    }
    serde_json::from_value(v).map_err(|e| ApiError::BadRequest(format!("invalid parameters: {e}")))
}

fn kind_of(name: &str) -> ApiResult<RequestKind> {
    match name {
        "create" => Ok(RequestKind::Create),
        "import" | "import_scan" => Ok(RequestKind::ImportScan),
        other => Err(ApiError::BadRequest(format!("unknown request type '{other}'"))),
    }
}

async fn list_accounts(State(s): State<Arc<AdminState>>) -> ApiResult<Response> {
    let mut groups = json!({"active": [], "inactive": [], "hidden": []});
    for a in s.app.primary.list_accounts().await? {
        let group = match a.status {
            AccountStatus::Active => "active",
            AccountStatus::Inactive => "inactive",
            AccountStatus::Hidden => "hidden",
        };
        groups[group].as_array_mut().unwrap().push(json!({
            "address": a.address,
            "scan_height": a.scan_height,
            "start_height": a.start_height,
            "access_time": a.last_access,
            "creation_time": a.created_at,
            "admin": a.is_admin(),
        }));
    }
    Ok(json_response(json_bytes(&s.app, &groups)?))
}

async fn list_requests(State(s): State<Arc<AdminState>>) -> ApiResult<Response> {
    let mut groups = json!({"create": [], "import": []});
    for r in s.app.primary.list_requests().await? {
        let group = if r.kind == RequestKind::Create { "create" } else { "import" };
        groups[group].as_array_mut().unwrap().push(json!({
            "address": r.address,
            "start_height": r.start_height,
            "creation_time": r.created_at,
        }));
    }
    Ok(json_response(json_bytes(&s.app, &groups)?))
}

#[derive(Deserialize)]
struct AddAccount {
    address: String,
    key: String,
    #[serde(default)]
    start_height: Option<u64>,
    #[serde(default)]
    admin: bool,
}

async fn add_account(State(s): State<Arc<AdminState>>, body: Bytes) -> ApiResult<Response> {
    let p: AddAccount = params(&body)?;
    let network = s.app.config.network;
    let (view_public, spend_public) =
        lws_kernel::decode_address(&p.address, network).map_err(|_| ApiError::BadRequest("invalid address".into()))?;
    let key = hex32(&p.key).ok_or_else(|| ApiError::BadRequest("invalid key".into()))?;
    if !lws_kernel::view_key_matches(&key, &view_public) {
        return Err(ApiError::BadRequest("view key does not match address".into()));
    }
    let start_height = match p.start_height {
        Some(h) => h,
        None => s.app.chain_tip().unwrap_or(0),
    };
    let new = NewAccount {
        address: lws_kernel::encode_address(&view_public, &spend_public, network),
        view_public,
        spend_public,
        view_key: ViewKey::new(key),
        status: AccountStatus::Active,
        flags: if p.admin { account_flags::ADMIN } else { 0 },
        start_height,
    };
    let created = matches!(s.app.primary.create_account(&new).await?, CreateOutcome::Created(_));
    info!(address = %new.address, created, start_height, "admin add_account");
    Ok(json_response(json_bytes(&s.app, &json!({"created": created, "start_height": start_height}))?))
}

#[derive(Deserialize)]
struct Requests {
    #[serde(rename = "type")]
    kind: String,
    addresses: Vec<String>,
}

async fn accept_requests(State(s): State<Arc<AdminState>>, body: Bytes) -> ApiResult<Response> {
    let p: Requests = params(&body)?;
    let accepted = s.app.primary.accept_requests(kind_of(&p.kind)?, &p.addresses).await?;
    info!(count = accepted.len(), kind = %p.kind, "admin accepted requests");
    Ok(json_response(json_bytes(&s.app, &json!({"updated": accepted}))?))
}

async fn reject_requests(State(s): State<Arc<AdminState>>, body: Bytes) -> ApiResult<Response> {
    let p: Requests = params(&body)?;
    let removed = s.app.primary.reject_requests(kind_of(&p.kind)?, &p.addresses).await?;
    Ok(json_response(json_bytes(&s.app, &json!({"removed": removed}))?))
}

#[derive(Deserialize)]
struct ModifyStatus {
    status: String,
    addresses: Vec<String>,
}

async fn modify_account_status(State(s): State<Arc<AdminState>>, body: Bytes) -> ApiResult<Response> {
    let p: ModifyStatus = params(&body)?;
    let status: AccountStatus = p.status.parse().map_err(ApiError::BadRequest)?;
    let updated = s.app.primary.set_status(&p.addresses, status).await?;
    info!(updated, status = %p.status, "admin modified account status");
    Ok(json_response(json_bytes(&s.app, &json!({"updated": updated}))?))
}

#[derive(Deserialize)]
struct Rescan {
    height: u64,
    addresses: Vec<String>,
}

async fn rescan(State(s): State<Arc<AdminState>>, body: Bytes) -> ApiResult<Response> {
    let p: Rescan = params(&body)?;
    let updated = s.app.primary.rescan(&p.addresses, p.height).await?;
    info!(updated, height = p.height, "admin rescan");
    Ok(json_response(json_bytes(&s.app, &json!({"updated": updated}))?))
}

async fn metrics(State(s): State<Arc<AdminState>>) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], s.app.metrics.render()).into_response()
}

/// Liveness plus detail for operators.
async fn health(State(s): State<Arc<AdminState>>) -> ApiResult<Response> {
    let db = s.app.primary.ping().await.is_ok();
    let replica = s.app.read.ping().await.is_ok();
    let tip = s.app.primary.tip().await.ok().flatten().map(|b| b.height);
    let body = json!({
        "status": if db && replica { "ok" } else { "degraded" },
        "database": {"backend": s.app.primary.backend(), "primary": db, "read": replica, "stored_tip": tip},
        "daemon": {"height": lws_core::metrics::Metrics::get(&s.app.metrics.daemon_height),
                   "endpoints": s.app.daemon.daemon.status()},
        "scanner": {"min_scan_height": lws_core::metrics::Metrics::get(&s.app.metrics.min_scan_height),
                    "accounts": lws_core::metrics::Metrics::get(&s.app.metrics.scanned_accounts)},
        "time": now_secs(),
    });
    let status = if db { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    Ok((status, json_response(json_bytes(&s.app, &body)?)).into_response())
}

/// Ready to take traffic: database reachable and at least one daemon healthy.
async fn ready(State(s): State<Arc<AdminState>>) -> StatusCode {
    let db = s.app.read.ping().await.is_ok();
    let daemon = s.app.daemon.daemon.status().iter().any(|e| e.healthy);
    if db && daemon {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub fn admin_router(state: Arc<AdminState>) -> Router {
    let protected = Router::new()
        .route("/list_accounts", post(list_accounts))
        .route("/list_requests", post(list_requests))
        .route("/add_account", post(add_account))
        .route("/accept_requests", post(accept_requests))
        .route("/reject_requests", post(reject_requests))
        .route("/modify_account_status", post(modify_account_status))
        .route("/rescan", post(rescan))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize))
        .layer(DefaultBodyLimit::max(1024 * 1024));
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .merge(protected)
        .with_state(state)
}
