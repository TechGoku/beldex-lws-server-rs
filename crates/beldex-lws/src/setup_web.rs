//! `beldex-lws setup --web`: the same setup in a browser.
//!
//! A short-lived server on loopback serves a form that detects beldexd, tests
//! the database and TLS files, and writes the configuration through the same
//! code as the terminal setup ([`crate::setup::write_plan`]); then it exits.
//!
//! The URL carries a one-time token in its fragment (never sent in requests
//! or logged); every API call must present it. Requests must name this
//! listener in their Host header, which stops other web pages from reaching
//! it through DNS rebinding. On a remote server, reach it through an SSH
//! tunnel; it never listens on a public address.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lws_core::Network;
use serde::Deserialize;
use serde_json::{json, Value};
use subtle::ConstantTimeEq;
use tokio::sync::watch;

use crate::settings::{self, default_daemon, is_root};
use crate::setup::{self, Plan, SetupArgs};

const SETUP_HTML: &str = include_str!("../assets/setup/index.html");
const SETUP_JS: &str = include_str!("../assets/setup/setup.js");

struct WebState {
    token: String,
    addr: SocketAddr,
    defaults: SetupArgs,
    done: watch::Sender<bool>,
}

type Shared = Arc<WebState>;

fn fail(status: StatusCode, message: impl std::fmt::Display) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

/// Host header and token checks for the API.
async fn guard(State(s): State<Shared>, req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let port = s.addr.port();
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
    if !allowed.iter().any(|a| a == host) {
        return fail(StatusCode::FORBIDDEN, "unexpected Host header");
    }
    if req.uri().path().starts_with("/api/") {
        let given = req.headers().get("x-setup-token").and_then(|v| v.to_str().ok()).unwrap_or("");
        if given.len() != s.token.len() || !bool::from(given.as_bytes().ct_eq(s.token.as_bytes())) {
            return fail(StatusCode::UNAUTHORIZED, "this setup link is not valid; use the one printed in the terminal");
        }
    }
    let mut response = next.run(req).await;
    let h = response.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, lws_api::ui::CSP.parse().unwrap());
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    h.insert(header::X_FRAME_OPTIONS, "DENY".parse().unwrap());
    h.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    h.entry(header::CACHE_CONTROL).or_insert("no-store".parse().unwrap());
    response
}

async fn detect(State(s): State<Shared>) -> Json<Value> {
    let mut candidates: Vec<String> = s.defaults.daemon.clone();
    for n in [Network::Mainnet, Network::Testnet, Network::Devnet] {
        let url = default_daemon(n);
        if !candidates.contains(&url) {
            candidates.push(url);
        }
    }
    let probes = futures_probe(&candidates).await;
    let data_dir = s.defaults.data_dir.clone().unwrap_or_else(settings::default_data_dir);
    let out = s.defaults.out.clone().unwrap_or_else(|| setup::default_config_path(&data_dir));
    let sockets: Value = [Network::Mainnet, Network::Testnet, Network::Devnet]
        .iter()
        .map(|n| (n.daemon_name().to_owned(), json!(settings::detect_notify_socket(*n))))
        .collect::<serde_json::Map<_, _>>()
        .into();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "root": is_root(),
        "daemons": probes,
        "data_dir": data_dir,
        "out": out,
        "out_exists": out.exists(),
        "network": s.defaults.network.map(|n| n.daemon_name()),
        "notify_sockets": sockets,
    }))
}

async fn futures_probe(urls: &[String]) -> Vec<Value> {
    let tasks: Vec<_> = urls
        .iter()
        .map(|u| {
            let u = u.clone();
            tokio::spawn(async move {
                match setup::probe_daemon(&u).await {
                    Ok(info) => json!({"url": u, "ok": true, "network": info.nettype, "height": info.height,
                                       "syncing": info.target_height > info.height + 5,
                                       "peers": info.outgoing_connections + info.incoming_connections}),
                    Err(e) => json!({"url": u, "ok": false, "error": e.to_string()}),
                }
            })
        })
        .collect();
    let mut out = Vec::new();
    for t in tasks {
        if let Ok(v) = t.await {
            out.push(v);
        }
    }
    out
}

#[derive(Deserialize)]
struct Url {
    url: String,
}

async fn check_daemon(Json(p): Json<Url>) -> Json<Value> {
    let url = p.url.trim().to_owned();
    Json(futures_probe(&[url]).await.pop().unwrap_or(Value::Null))
}

async fn check_database(Json(p): Json<Url>) -> Json<Value> {
    match setup::check_database(p.url.trim()).await {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": format!("{e:#}")})),
    }
}

#[derive(Deserialize)]
struct Tls {
    cert: PathBuf,
    key: PathBuf,
}

async fn check_tls(Json(p): Json<Tls>) -> Json<Value> {
    match setup::check_tls(&p.cert, &p.key).await {
        Ok(()) => Json(json!({"ok": true})),
        Err(e) => Json(json!({"ok": false, "error": format!("{e:#}")})),
    }
}

#[derive(Deserialize)]
struct Apply {
    network: String,
    daemons: Vec<String>,
    database: Option<String>,
    /// local, proxy or tls.
    access: String,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    port: Option<u16>,
    account_creation: String,
    data_dir: PathBuf,
    out: PathBuf,
    #[serde(default)]
    replace: bool,
}

fn plan_from(a: Apply) -> Result<Plan> {
    let network: Network = a.network.parse().map_err(anyhow::Error::msg)?;
    let daemons: Vec<String> = a.daemons.iter().map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()).collect();
    let port = a.port.unwrap_or(8443);
    if port == 8444 {
        bail!("port 8444 is the admin dashboard's; pick another for wallets");
    }
    let (listen, tls, behind_proxy) = match a.access.as_str() {
        "local" => (SocketAddr::from(([127, 0, 0, 1], port)), None, false),
        "proxy" => (SocketAddr::from(([127, 0, 0, 1], port)), None, true),
        "tls" => {
            let cert = a.tls_cert.context("the certificate file is missing")?;
            let key = a.tls_key.context("the key file is missing")?;
            (SocketAddr::from(([0, 0, 0, 0], port)), Some((cert, key)), false)
        }
        other => bail!("unknown access mode {other}"),
    };
    if !a.data_dir.is_absolute() || !a.out.is_absolute() {
        bail!("use absolute paths for the data directory and the config file");
    }
    Ok(Plan {
        network,
        daemons,
        database: a.database.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()),
        listen,
        tls,
        behind_proxy,
        account_creation: a.account_creation,
        data_dir: a.data_dir,
        out: a.out,
        force: a.replace,
    })
}

async fn apply(State(s): State<Shared>, Json(a): Json<Apply>) -> Response {
    let plan = match plan_from(a) {
        Ok(p) => p,
        Err(e) => return fail(StatusCode::BAD_REQUEST, format!("{e:#}")),
    };
    // The daemon must run the chosen network when it answers at all.
    if let Some(first) = plan.daemons.first() {
        if let Ok(info) = setup::probe_daemon(first).await {
            if info.nettype != plan.network.daemon_name() {
                return fail(
                    StatusCode::BAD_REQUEST,
                    format!("{first} runs {}, not {}", info.nettype, plan.network.daemon_name()),
                );
            }
        }
    }
    if let Some(url) = &plan.database {
        if let Err(e) = setup::check_database(url).await {
            return fail(StatusCode::BAD_REQUEST, format!("database: {e:#}"));
        }
    }
    if let Err(e) = setup::check_plan(&plan).await {
        return fail(StatusCode::BAD_REQUEST, format!("{e:#}"));
    }
    let written = match setup::write_plan(&plan) {
        Ok(w) => w,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    };
    eprintln!("Wrote {}", written.out.display());
    for path in &written.created {
        eprintln!("Created {}", path.display());
    }
    let out = written.out.display().to_string();
    let body = json!({
        "out": out,
        "created": written.created,
        "config": written.text,
        "seal_key": written.seal_key,
        "token_file": written.token,
        "wallet_url": format!("{}://{}", if plan.tls.is_some() { "https" } else { "http" }, plan.listen),
        "dashboard_url": "http://127.0.0.1:8444/",
        "commands": [format!("beldex-lws doctor --config {out}"), format!("beldex-lws serve --config {out}")],
        "service": out == "/etc/beldex-lws/lws.toml",
    });
    // Answer, then stop: the link is single-use.
    let done = s.done.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let _ = done.send(true);
    });
    Json(body).into_response()
}

fn router(state: Shared) -> Router {
    Router::new()
        .route("/", get(|| async { lws_api::ui::page(SETUP_HTML, &[lws_api::ui::CSS, SETUP_JS]) }))
        .route("/ui/setup.js", get(|| async { lws_api::ui::asset("text/javascript; charset=utf-8", SETUP_JS) }))
        .route("/ui/ui.css", get(|| async { lws_api::ui::asset("text/css; charset=utf-8", lws_api::ui::CSS) }))
        .route("/api/detect", get(detect))
        .route("/api/check/daemon", post(check_daemon))
        .route("/api/check/database", post(check_database))
        .route("/api/check/tls", post(check_tls))
        .route("/api/apply", post(apply))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

pub async fn run(args: SetupArgs) -> Result<()> {
    let addr = args.web_listen;
    if !addr.ip().is_loopback() {
        bail!("--web-listen must be a loopback address; reach it from elsewhere through an SSH tunnel");
    }
    lws_kernel::self_check().context("the scan kernel failed its self-check; reinstall the server")?;
    let mut secret = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rng(), &mut secret);
    let token = hex::encode(secret);
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
    let addr = listener.local_addr()?;
    let (done, mut stopped) = watch::channel(false);
    let state = Arc::new(WebState { token: token.clone(), addr, defaults: args, done });

    let app = router(state);

    let url = format!("http://{addr}/#{token}");
    eprintln!("Open this link in a browser on this machine to set up beldex-lws:\n\n    {url}\n");
    eprintln!(
        "On a remote server, forward the port first:  ssh -L {port}:127.0.0.1:{port} <server>\n\
         then open the same link locally. The link stops working once the configuration is written.\n",
        port = addr.port()
    );
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let _ = std::process::Command::new("xdg-open")
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = stopped.wait_for(|d| *d) => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await?;
    eprintln!("Setup finished.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use clap::Parser;
    use tower::ServiceExt;

    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        args: SetupArgs,
    }

    fn state(dir: &std::path::Path) -> Shared {
        let args = Wrap::parse_from(["x", "--data-dir", dir.to_str().unwrap()]).args;
        let (done, _) = watch::channel(false);
        Arc::new(WebState { token: "t0ken".into(), addr: "127.0.0.1:8445".parse().unwrap(), defaults: args, done })
    }

    async fn send(
        app: &Router,
        method: &str,
        path: &str,
        host: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut req = axum::http::Request::builder().method(method).uri(path).header("host", host);
        if let Some(t) = token {
            req = req.header("x-setup-token", t);
        }
        let req = if method == "POST" {
            req.header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
        } else {
            req.body(Body::empty()).unwrap()
        };
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lws-setup-web-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn only_this_listener_with_the_token_gets_in() {
        let dir = temp_dir("guard");
        let app = router(state(&dir));
        assert_eq!(send(&app, "GET", "/", "evil.example:8445", None, Value::Null).await.0, StatusCode::FORBIDDEN);
        assert_eq!(send(&app, "GET", "/", "127.0.0.1:8445", None, Value::Null).await.0, StatusCode::OK);
        assert_eq!(
            send(&app, "GET", "/api/detect", "127.0.0.1:8445", None, Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send(&app, "GET", "/api/detect", "localhost:8445", Some("wrong"), Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
        let (s, d) = send(&app, "GET", "/api/detect", "localhost:8445", Some("t0ken"), Value::Null).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(d["data_dir"], dir.to_str().unwrap());
        assert_eq!(d["daemons"].as_array().unwrap().len(), 3, "the three default ports are probed");
    }

    #[tokio::test]
    async fn apply_writes_the_configuration_once() {
        let dir = temp_dir("apply");
        let app = router(state(&dir));
        let plan = json!({
            "network": "testnet", "daemons": ["http://127.0.0.1:1"], "database": null, "access": "proxy",
            "port": 9443, "account_creation": "manual", "data_dir": dir, "out": dir.join("lws.toml"),
        });
        let (s, r) = send(&app, "POST", "/api/apply", "127.0.0.1:8445", Some("t0ken"), plan.clone()).await;
        assert_eq!(s, StatusCode::OK, "{r}");
        let text = std::fs::read_to_string(dir.join("lws.toml")).unwrap();
        assert!(text.contains("network = \"testnet\""));
        assert!(text.contains("listen = \"127.0.0.1:9443\""));
        assert!(text.contains("trust-forwarded-for = true"));
        assert!(text.contains("account-creation = \"manual\""));
        assert!(dir.join(crate::settings::SEALING_KEY_FILE).exists());
        assert_eq!(r["created"].as_array().unwrap().len(), 2);

        // A second write needs explicit consent.
        let (s, r) = send(&app, "POST", "/api/apply", "127.0.0.1:8445", Some("t0ken"), plan.clone()).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(r["error"].as_str().unwrap().contains("already exists"), "{r}");

        for (field, value, expect) in [
            ("port", json!(8444), "admin dashboard"),
            ("access", json!("open"), "unknown access"),
            ("out", json!("relative.toml"), "absolute"),
            ("network", json!("moon"), "network"),
        ] {
            let mut bad = plan.clone();
            bad[field] = value;
            bad["replace"] = json!(true);
            let (s, r) = send(&app, "POST", "/api/apply", "127.0.0.1:8445", Some("t0ken"), bad).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{field}");
            assert!(r["error"].as_str().unwrap().to_lowercase().contains(expect), "{field}: {r}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
