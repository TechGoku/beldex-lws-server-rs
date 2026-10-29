//! The web interface: the admin dashboard, compiled into the binary and
//! served by the admin listener. Plain HTML, CSS and JavaScript with no
//! external requests, under a strict Content-Security-Policy. The page holds
//! no secrets; it asks for the admin token and sends it as a bearer token on
//! each API call, so there are no cookies and no CSRF surface.

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

/// Shared by the dashboard and `beldex-lws setup --web`.
pub const CSS: &str = include_str!("../assets/ui.css");
const DASHBOARD_HTML: &str = include_str!("../assets/dashboard/index.html");
const DASHBOARD_JS: &str = include_str!("../assets/dashboard/app.js");

pub const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                       connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// A static asset with its content type. Assets are versioned by query
/// string, so they may be cached.
pub fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "public, max-age=3600")], body).into_response()
}

/// The page, with `{{VERSION}}` replaced by a fingerprint of its assets, so
/// asset URLs change whenever the assets do and caches never serve stale ones.
pub fn page(html: &'static str, assets: &[&str]) -> Response {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for a in assets {
        a.hash(&mut hasher);
    }
    let tag = format!("{}-{:016x}", env!("CARGO_PKG_VERSION"), hasher.finish());
    let body = html.replace("{{VERSION}}", &tag);
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

pub fn dashboard_routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(|| async { page(DASHBOARD_HTML, &[CSS, DASHBOARD_JS]) }))
        .route("/dashboard", get(|| async { page(DASHBOARD_HTML, &[CSS, DASHBOARD_JS]) }))
        .route("/ui/app.js", get(|| async { asset("text/javascript; charset=utf-8", DASHBOARD_JS) }))
        .route("/ui/ui.css", get(|| async { asset("text/css; charset=utf-8", CSS) }))
}

/// Security headers for every admin response; API responses are never cached.
pub async fn admin_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    h.entry(header::CACHE_CONTROL).or_insert(HeaderValue::from_static("no-store"));
    response
}
