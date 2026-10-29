//! The admin JSON API behind the dashboard, under `/api/v1`. Same
//! authorisation as the rest of the admin API. Unlike the LMDB-compatible
//! routes it pages through accounts (keyset, newest first), never returns
//! view keys, reports errors as `{"error": "..."}`, and records every change
//! in the audit log.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use lws_core::metrics::Metrics;
use lws_core::{account_flags, now_secs, AccountStatus, RequestKind, ViewKey};
use lws_store::{AccountFilter, AccountSummary, CreateOutcome, NewAccount};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::{audit, AdminState, Who};
use crate::error::ApiError;
use crate::wire::hex32;

/// Accounts this many blocks below the chain tip count as behind.
const BEHIND_BLOCKS: u64 = 10;
/// An instance silent for longer than this is shown as stale.
const STALE_AFTER: i64 = 60;

/// Errors as JSON. Client errors carry their message; server errors do not
/// leak detail (it is logged instead).
pub struct V1Error(ApiError);

impl From<ApiError> for V1Error {
    fn from(e: ApiError) -> Self {
        V1Error(e)
    }
}

impl From<lws_store::Error> for V1Error {
    fn from(e: lws_store::Error) -> Self {
        V1Error(e.into())
    }
}

impl IntoResponse for V1Error {
    fn into_response(self) -> Response {
        let status = self.0.status();
        let message = match &self.0 {
            ApiError::BadRequest(m) => m.clone(),
            ApiError::Unavailable(m) => format!("unavailable: {m}"),
            other if status.is_server_error() => {
                tracing::error!(error = %other, "admin request failed");
                "internal error; see the server log".into()
            }
            other => other.to_string(),
        };
        (status, Json(json!({"error": message}))).into_response()
    }
}

type V1<T = Json<Value>> = Result<T, V1Error>;

fn bad(message: impl Into<String>) -> V1Error {
    V1Error(ApiError::BadRequest(message.into()))
}

fn status_name(s: AccountStatus) -> &'static str {
    match s {
        AccountStatus::Active => "active",
        AccountStatus::Inactive => "inactive",
        AccountStatus::Hidden => "hidden",
    }
}

fn account_json(a: &AccountSummary, tip: Option<u64>) -> Value {
    json!({
        "id": a.id,
        "address": a.address,
        "status": status_name(a.status),
        "admin": a.flags & account_flags::ADMIN != 0,
        "generated_locally": a.flags & account_flags::GENERATED_LOCALLY != 0,
        "start_height": a.start_height,
        "scan_height": a.scan_height,
        // scan_height is the last block scanned.
        "behind": tip.map(|t| t.saturating_sub(a.scan_height)),
        "created_at": a.created_at,
        "last_access": a.last_access,
    })
}

/// The whole picture in one call: this process, the chain, the daemons,
/// account and request counts, and every instance of the deployment.
pub async fn overview(State(s): State<Arc<AdminState>>) -> V1 {
    let app = &s.app;
    let tip = app.chain_tip();
    let height = Metrics::get(&app.metrics.daemon_height);
    let counts = app.primary.account_counts(height.saturating_sub(BEHIND_BLOCKS)).await?;
    let stored_tip = app.primary.tip().await?.map(|b| b.height);
    let db_ok = app.primary.ping().await.is_ok();
    let replica_ok = app.read.ping().await.is_ok();
    let now = now_secs();
    let instances: Vec<Value> = app
        .primary
        .instances(now - 24 * 3600)
        .await?
        .into_iter()
        .map(|i| {
            json!({
                "name": i.name,
                "role": i.role,
                "version": i.version,
                "started_at": i.started_at,
                "heartbeat_at": i.heartbeat_at,
                "stale": now - i.heartbeat_at > STALE_AFTER,
                "stats": serde_json::from_str::<Value>(&i.stats).unwrap_or(Value::Null),
            })
        })
        .collect();
    let leases: Vec<Value> = app
        .primary
        .leases()
        .await?
        .into_iter()
        .map(|l| json!({"name": l.name, "holder": l.holder, "expires_at": l.expires_at, "live": l.expires_at >= now}))
        .collect();
    let min_scan = Metrics::get(&app.metrics.min_scan_height);
    let status = if !db_ok {
        "down"
    } else if !replica_ok || !app.daemon.daemon.status().iter().any(|e| e.healthy) {
        "degraded"
    } else {
        "ok"
    };
    Ok(Json(json!({
        "status": status,
        "time": now,
        "server": s.info,
        "network": app.config.network.daemon_name(),
        "database": {"backend": app.primary.backend(), "primary": db_ok, "read": replica_ok, "stored_tip": stored_tip},
        "chain": {"height": height, "tip": tip},
        "daemons": app.daemon.daemon.status(),
        "accounts": {
            "active": counts.active, "inactive": counts.inactive, "hidden": counts.hidden,
            "total": counts.active + counts.inactive + counts.hidden,
            "behind": counts.active_behind, "behind_blocks": BEHIND_BLOCKS,
        },
        "requests": {"create": counts.pending_create, "import": counts.pending_import},
        "scanner": {"min_scan_height": min_scan, "lag": height.saturating_sub(min_scan + 1)},
        "metrics": app.metrics.snapshot(),
        "instances": instances,
        "leases": leases,
    })))
}

#[derive(Deserialize)]
pub struct AccountQuery {
    status: Option<String>,
    q: Option<String>,
    /// Only accounts behind the chain tip.
    #[serde(default)]
    behind: bool,
    before: Option<i64>,
    limit: Option<u32>,
}

pub async fn accounts(State(s): State<Arc<AdminState>>, Query(p): Query<AccountQuery>) -> V1 {
    let status = match p.status.as_deref().filter(|v| !v.is_empty() && *v != "all") {
        Some(v) => Some(v.parse::<AccountStatus>().map_err(bad)?),
        None => None,
    };
    let tip = s.app.chain_tip();
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let filter = AccountFilter {
        status,
        search: p.q,
        scanned_below: p.behind.then(|| Metrics::get(&s.app.metrics.daemon_height).saturating_sub(BEHIND_BLOCKS)),
        before_id: p.before,
        limit,
    };
    let page = s.app.read.account_page(&filter).await?;
    let next = (page.len() == limit as usize).then(|| page.last().map(|a| a.id)).flatten();
    Ok(Json(json!({
        "accounts": page.iter().map(|a| account_json(a, tip)).collect::<Vec<_>>(),
        "next_before": next,
    })))
}

pub async fn account(State(s): State<Arc<AdminState>>, Path(address): Path<String>) -> V1 {
    let Some(a) = s.app.read.account_summary(&address).await? else {
        return Err(V1Error(ApiError::BadRequest("no such account".into())));
    };
    let (outputs, spends) = s.app.read.account_activity(a.id).await?;
    let mut body = account_json(&a, s.app.chain_tip());
    body["outputs"] = outputs.into();
    body["spends"] = spends.into();
    Ok(Json(body))
}

#[derive(Deserialize)]
pub struct AddAccount {
    address: String,
    view_key: String,
    start_height: Option<u64>,
    #[serde(default)]
    admin: bool,
}

pub async fn add_account(
    State(s): State<Arc<AdminState>>,
    Extension(who): Extension<Who>,
    Json(p): Json<AddAccount>,
) -> V1<Response> {
    let network = s.app.config.network;
    let (view_public, spend_public) =
        lws_kernel::decode_address(p.address.trim(), network).map_err(|_| bad(format!("not a {network} address")))?;
    let key = hex32(p.view_key.trim()).ok_or_else(|| bad("the view key must be 64 hex characters"))?;
    if !lws_kernel::view_key_matches(&key, &view_public) {
        return Err(bad("the view key does not belong to this address"));
    }
    let start_height = p.start_height.or_else(|| s.app.chain_tip()).unwrap_or(0);
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
    if created {
        audit(&s, &who, "add_account", json!({"address": new.address, "start_height": start_height, "admin": p.admin}))
            .await;
    }
    let code = if created { StatusCode::CREATED } else { StatusCode::OK };
    Ok((code, Json(json!({"created": created, "address": new.address, "start_height": start_height}))).into_response())
}

#[derive(Deserialize)]
pub struct SetStatus {
    status: String,
    addresses: Vec<String>,
}

fn check_addresses(addresses: &[String]) -> Result<(), V1Error> {
    match addresses.len() {
        0 => Err(bad("no addresses given")),
        n if n > 10_000 => Err(bad("at most 10000 addresses per call")),
        _ => Ok(()),
    }
}

/// The addresses for the audit record: all of them up to 20, then a count.
fn listed(addresses: &[String]) -> Value {
    json!({"count": addresses.len(), "addresses": addresses.iter().take(20).collect::<Vec<_>>()})
}

pub async fn set_status(
    State(s): State<Arc<AdminState>>,
    Extension(who): Extension<Who>,
    Json(p): Json<SetStatus>,
) -> V1 {
    check_addresses(&p.addresses)?;
    let status: AccountStatus = p.status.parse().map_err(bad)?;
    let updated = s.app.primary.set_status(&p.addresses, status).await?;
    audit(&s, &who, "set_status", json!({"status": p.status, "updated": listed(&updated)})).await;
    Ok(Json(json!({"updated": updated})))
}

#[derive(Deserialize)]
pub struct Rescan {
    height: u64,
    addresses: Vec<String>,
}

pub async fn rescan(State(s): State<Arc<AdminState>>, Extension(who): Extension<Who>, Json(p): Json<Rescan>) -> V1 {
    check_addresses(&p.addresses)?;
    let updated = s.app.primary.rescan(&p.addresses, p.height).await?;
    audit(&s, &who, "rescan", json!({"height": p.height, "updated": listed(&updated)})).await;
    Ok(Json(json!({"updated": updated})))
}

pub async fn requests(State(s): State<Arc<AdminState>>) -> V1 {
    let rows: Vec<Value> = s
        .app
        .primary
        .list_requests()
        .await?
        .iter()
        .map(|r| {
            json!({
                "kind": if r.kind == RequestKind::Create { "create" } else { "import" },
                "address": r.address,
                "start_height": r.start_height,
                "created_at": r.created_at,
            })
        })
        .collect();
    Ok(Json(json!({"requests": rows})))
}

#[derive(Deserialize)]
pub struct Decide {
    kind: String,
    addresses: Vec<String>,
}

fn kind(name: &str) -> Result<RequestKind, V1Error> {
    match name {
        "create" => Ok(RequestKind::Create),
        "import" => Ok(RequestKind::ImportScan),
        other => Err(bad(format!("unknown request kind '{other}' (create or import)"))),
    }
}

pub async fn accept(State(s): State<Arc<AdminState>>, Extension(who): Extension<Who>, Json(p): Json<Decide>) -> V1 {
    check_addresses(&p.addresses)?;
    let done = s.app.primary.accept_requests(kind(&p.kind)?, &p.addresses).await?;
    audit(&s, &who, "accept_requests", json!({"kind": p.kind, "updated": listed(&done)})).await;
    Ok(Json(json!({"updated": done})))
}

pub async fn reject(State(s): State<Arc<AdminState>>, Extension(who): Extension<Who>, Json(p): Json<Decide>) -> V1 {
    check_addresses(&p.addresses)?;
    let done = s.app.primary.reject_requests(kind(&p.kind)?, &p.addresses).await?;
    audit(&s, &who, "reject_requests", json!({"kind": p.kind, "updated": listed(&done)})).await;
    Ok(Json(json!({"updated": done})))
}

#[derive(Deserialize)]
pub struct Page {
    before: Option<i64>,
    limit: Option<u32>,
}

pub async fn audit_log(State(s): State<Arc<AdminState>>, Query(p): Query<Page>) -> V1 {
    let limit = p.limit.unwrap_or(50).clamp(1, 500);
    let entries = s.app.primary.audit_log(p.before, limit).await?;
    let next = (entries.len() == limit as usize).then(|| entries.last().map(|e| e.id)).flatten();
    let rows: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "id": e.id, "at": e.at, "actor": e.actor, "remote": e.remote, "action": e.action,
                "detail": serde_json::from_str::<Value>(&e.detail).unwrap_or(Value::String(e.detail.clone())),
            })
        })
        .collect();
    Ok(Json(json!({"entries": rows, "next_before": next})))
}

/// The effective settings of this process, without secrets.
pub async fn config(State(s): State<Arc<AdminState>>) -> V1 {
    let c = &s.app.config;
    Ok(Json(json!({
        "server": s.info,
        "api": {
            "network": c.network.daemon_name(),
            "account_creation": format!("{:?}", c.creation).to_lowercase(),
            "auto_accept_imports": c.auto_accept_imports,
            "rate_limit": c.rate_limit,
            "rate_burst": c.rate_burst,
            "creations_per_hour": c.creations_per_hour,
            "max_in_flight": c.max_in_flight,
            "request_timeout_s": c.request_timeout.as_secs(),
            "trust_forwarded_for": c.trust_forwarded_for,
            "cors_origins": c.cors_origins,
            "response_cache_mb": c.response_cache_bytes / (1024 * 1024),
            "max_response_mb": c.max_response_bytes / (1024 * 1024),
        },
    })))
}
