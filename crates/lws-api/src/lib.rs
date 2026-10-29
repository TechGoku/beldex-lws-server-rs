//! The lightwallet REST API (public) and the admin/ops API (private).
//!
//! The public router is stateless apart from caches, so any number of replicas
//! can run behind a load balancer against one database (reads may go to a
//! replica). Each replica protects itself: per-route body limits, a request
//! timeout, a global concurrency cap that sheds excess load with 503 instead
//! of queueing it, per-client rate limits, and a separate, stricter limit on
//! account creation.

pub mod admin;
pub mod admin_v1;
pub mod daemon_data;
pub mod error;
pub mod limits;
pub mod random_outs;
pub mod ui;
pub mod wallet;
pub mod wire;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::error_handling::HandleErrorLayer;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{BoxError, Router};
use bytes::Bytes;
use lws_core::metrics::Metrics;
use lws_core::Network;
use lws_daemon::Daemon;
use lws_store::Store;
use tower::ServiceBuilder;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::daemon_data::DaemonData;
use crate::error::ApiError;
use crate::limits::{ClientKey, RateLimiter};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Creation {
    /// `/login` creates accounts immediately.
    Auto,
    /// `/login` files a request an operator approves.
    Manual,
    /// `/login` never creates accounts (501).
    Disabled,
}

impl std::str::FromStr for Creation {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Creation::Auto),
            "manual" => Ok(Creation::Manual),
            "disabled" | "off" => Ok(Creation::Disabled),
            other => Err(format!("unknown account-creation mode '{other}' (auto, manual, disabled)")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub network: Network,
    pub creation: Creation,
    /// Rescan requests from `/import_request` are applied without approval.
    pub auto_accept_imports: bool,
    pub request_timeout: Duration,
    /// Requests served at once per replica; the rest get 503 immediately.
    pub max_in_flight: usize,
    /// Requests per second per client, and the burst allowance.
    pub rate_limit: f64,
    pub rate_burst: f64,
    /// Account creations per hour per client.
    pub creations_per_hour: f64,
    /// Use the right-most `X-Forwarded-For` entry as the client address.
    /// Only enable behind a proxy that sets it.
    pub trust_forwarded_for: bool,
    /// Origins allowed by CORS; empty disables CORS headers, `*` allows any.
    pub cors_origins: Vec<String>,
    /// Bytes of rendered responses cached per replica.
    pub response_cache_bytes: u64,
    /// Responses larger than this are refused with 503 (0 = unlimited).
    pub max_response_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: Network::Mainnet,
            creation: Creation::Auto,
            auto_accept_imports: false,
            request_timeout: Duration::from_secs(30),
            max_in_flight: 4096,
            rate_limit: 20.0,
            rate_burst: 60.0,
            creations_per_hour: 30.0,
            trust_forwarded_for: false,
            cors_origins: Vec::new(),
            response_cache_bytes: 256 * 1024 * 1024,
            max_response_bytes: 0,
        }
    }
}

pub struct AppState {
    pub config: Config,
    /// Writes and anything that must see its own writes.
    pub primary: Arc<Store>,
    /// Account reads; a replica when one is configured.
    pub read: Arc<Store>,
    pub daemon: Arc<DaemonData>,
    pub metrics: Arc<Metrics>,
    pub limiter: RateLimiter,
    pub creation_limiter: RateLimiter,
    /// Rendered responses keyed by endpoint, account, scan state and request.
    pub responses: moka::future::Cache<String, Bytes>,
}

impl AppState {
    pub fn new(
        config: Config,
        primary: Arc<Store>,
        read: Arc<Store>,
        daemon: Arc<Daemon>,
        metrics: Arc<Metrics>,
    ) -> Arc<AppState> {
        let responses = moka::future::Cache::builder()
            .weigher(|k: &String, v: &Bytes| (k.len() + v.len()).try_into().unwrap_or(u32::MAX))
            .max_capacity(config.response_cache_bytes)
            .time_to_live(Duration::from_secs(10))
            .build();
        Arc::new(AppState {
            limiter: RateLimiter::new(config.rate_limit, config.rate_burst),
            creation_limiter: RateLimiter::new(config.creations_per_hour / 3600.0, config.creations_per_hour.max(1.0)),
            config,
            primary,
            read,
            daemon: Arc::new(DaemonData::new(daemon)),
            metrics,
            responses,
        })
    }

    /// The highest block index the daemon has reported, if any.
    pub fn chain_tip(&self) -> Option<u64> {
        Metrics::get(&self.metrics.daemon_height).checked_sub(1)
    }
}

/// The address requests are attributed to for rate limiting.
#[derive(Clone, Copy, Debug)]
pub struct ClientAddr(pub IpAddr);

fn client_addr(state: &AppState, req: &Request) -> IpAddr {
    let peer =
        req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
    if state.config.trust_forwarded_for {
        if let Some(ip) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
    }
    peer
}

async fn guard(State(state): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    let ip = client_addr(&state, &req);
    req.extensions_mut().insert(ClientAddr(ip));
    if let Err(retry_after) = state.limiter.check(ClientKey::from(ip), 1.0) {
        Metrics::add(&state.metrics.http_rate_limited_total, 1);
        return ApiError::RateLimited { retry_after }.into_response();
    }

    Metrics::add(&state.metrics.http_requests_total, 1);
    state.metrics.http_in_flight.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let began = std::time::Instant::now();
    let response = next.run(req).await;
    state.metrics.http_request_duration_seconds.observe(began.elapsed());
    state.metrics.http_in_flight.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    let status = response.status();
    if status.is_client_error() {
        Metrics::add(&state.metrics.http_client_errors_total, 1);
    } else if status.is_server_error() {
        Metrics::add(&state.metrics.http_server_errors_total, 1);
    }
    response
}

async fn overloaded(state: Arc<AppState>, err: BoxError) -> Response {
    if err.is::<tower::load_shed::error::Overloaded>() {
        Metrics::add(&state.metrics.http_shed_total, 1);
        (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable").into_response()
    } else if err.is::<tower::timeout::error::Elapsed>() {
        (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable").into_response()
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
    }
}

fn cors(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }
    let allow = if origins.iter().any(|o| o == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(origins.iter().filter_map(|o| HeaderValue::from_str(o).ok()))
    };
    Some(
        CorsLayer::new()
            .allow_origin(allow)
            .allow_methods([Method::POST, Method::OPTIONS])
            .allow_headers([header::CONTENT_TYPE, header::ACCEPT_ENCODING])
            .max_age(Duration::from_secs(3600)),
    )
}

/// The public, wallet-facing router.
pub fn public_router(state: Arc<AppState>) -> Router {
    let kib = |n: usize| DefaultBodyLimit::max(n * 1024);
    let routes = Router::new()
        .route("/login", post(wallet::login).layer(kib(2)))
        .route("/get_address_info", post(wallet::get_address_info).layer(kib(2)))
        .route("/get_address_txs", post(wallet::get_address_txs).layer(kib(2)))
        .route("/get_unspent_outs", post(wallet::get_unspent_outs).layer(kib(2)))
        .route("/get_random_outs", post(wallet::get_random_outs).layer(kib(16)))
        .route("/import_request", post(wallet::import_request).layer(kib(2)))
        .route("/submit_raw_tx", post(wallet::submit_raw_tx).layer(kib(256)))
        .route("/daemon_status", post(wallet::daemon_status).layer(kib(1)))
        .route("/get_token_balances", post(wallet::get_token_balances).layer(kib(8)))
        .route("/get_token_info", post(wallet::get_token_info).layer(kib(1)))
        .route("/get_token_list", post(wallet::get_token_list).layer(kib(1)))
        .route("/health", get(wallet::health).post(wallet::health))
        .fallback(|| async { (StatusCode::NOT_FOUND, "Not Found") });

    let routes = match cors(&state.config.cors_origins) {
        Some(layer) => routes.layer(layer),
        None => routes,
    };

    let shed_state = state.clone();
    routes
        .layer(CompressionLayer::new().gzip(true))
        .layer(SetResponseHeaderLayer::overriding(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")))
        .layer(SetResponseHeaderLayer::overriding(header::CACHE_CONTROL, HeaderValue::from_static("no-store")))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(move |err: BoxError| overloaded(shed_state.clone(), err)))
                .load_shed()
                .concurrency_limit(state.config.max_in_flight.max(1))
                .timeout(state.config.request_timeout),
        )
        .with_state(state)
}

/// JSON response helper: serialises once, enforcing the size cap.
pub(crate) fn json_bytes<T: serde::Serialize>(state: &AppState, value: &T) -> Result<Bytes, ApiError> {
    let bytes = serde_json::to_vec(value).map_err(|e| ApiError::Internal(e.to_string()))?;
    if state.config.max_response_bytes != 0 && bytes.len() > state.config.max_response_bytes {
        return Err(ApiError::Unavailable(format!(
            "response of {} bytes exceeds the configured limit; client should paginate with max_count",
            bytes.len()
        )));
    }
    Ok(Bytes::from(bytes))
}

pub(crate) fn json_response(bytes: Bytes) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Background upkeep for one replica: tracking the daemon's height (so
/// API-only replicas report it without a scanner) and sweeping idle
/// rate-limit buckets.
pub async fn maintenance(state: Arc<AppState>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let mut height_tick = tokio::time::interval(Duration::from_secs(5));
    let mut sweep_tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = height_tick.tick() => {
                if let Ok(info) = state.daemon.daemon.get_info().await {
                    let seen = Metrics::get(&state.metrics.daemon_height);
                    Metrics::set(&state.metrics.daemon_height, seen.max(info.height));
                }
            }
            _ = sweep_tick.tick() => {
                state.limiter.sweep();
                state.creation_limiter.sweep();
            }
            _ = shutdown.changed() => break,
        }
    }
}

#[cfg(test)]
mod tests;
