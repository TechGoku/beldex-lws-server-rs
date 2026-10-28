//! `beldex-lws`: the Beldex light wallet server.
//!
//! One binary, several roles. `serve --role all` runs everything in one
//! process; at scale, run `--role api` replicas behind a load balancer and one
//! or more `--role scanner` processes (sharded with `--scan-shard`), all
//! against one PostgreSQL database.

mod admin_cli;
mod legacy;
mod settings;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lws_core::metrics::Metrics;
use lws_daemon::Daemon;
use lws_store::{Sealer, Store};
use tokio::sync::watch;
use tracing::{error, info, warn};

use crate::settings::{Role, ServeArgs, StoreArgs};

#[derive(Parser)]
#[command(name = "beldex-lws", version, about = "Beldex light wallet server", args_override_self = true)]
struct Cli {
    /// TOML file of option defaults, keyed by long option name
    /// (e.g. `database = "postgres://..."`). Environment and flags win.
    #[arg(long, global = true, env = "LWS_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (API, scanner, or both).
    Serve(Box<ServeArgs>),
    /// Apply database migrations and exit.
    Migrate(StoreArgs),
    /// Administer accounts directly in the database. Safe while the server runs.
    #[command(subcommand)]
    Admin(admin_cli::AdminCommand),
    /// Write a new random 32-byte key (for --sealing-key-file or --admin-token-file).
    Keygen {
        /// File to create; refuses to overwrite. Written with mode 0600.
        out: PathBuf,
    },
}

fn init_logging(format: &str, level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(format!("{level},sqlx=warn,hyper=warn,tower_http=warn"))
    });
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    if format == "json" {
        builder.json().flatten_event(true).init();
    } else {
        builder.init();
    }
}

/// Creates `path` with mode 0600, failing if it exists.
pub(crate) fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path).with_context(|| format!("creating {}", path.display()))?;
    f.write_all(contents)?;
    f.sync_all()?;
    Ok(())
}

/// Reads a secret file, refusing one other users can read.
pub(crate) fn read_secret_file(path: &std::path::Path) -> Result<zeroize::Zeroizing<String>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).with_context(|| format!("reading {}", path.display()))?.permissions().mode();
        if mode & 0o077 != 0 {
            bail!("{} is readable by other users (mode {:o}); chmod 600 it", path.display(), mode & 0o777);
        }
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(zeroize::Zeroizing::new(text.trim().to_owned()))
}

pub(crate) fn sealer(args: &StoreArgs) -> Result<Sealer> {
    let load = |path: &std::path::Path| -> Result<[u8; 32]> {
        let text = read_secret_file(path)?;
        lws_core::parse_key(&text).with_context(|| format!("{} must hold 64 hex characters", path.display()))
    };
    match &args.sealing_key_file {
        Some(path) => {
            let current = load(path)?;
            let mut previous = Vec::new();
            for spec in &args.sealing_key_old {
                let (id, path) = spec.split_once(':').context("--sealing-key-old takes ID:PATH")?;
                previous.push((id.parse().context("sealing key id must be 0-255")?, load(path.as_ref())?));
            }
            Ok(Sealer::new((args.sealing_key_id, &current), &previous))
        }
        None if args.allow_plaintext_keys => {
            warn!("view keys are stored UNENCRYPTED (--allow-plaintext-keys); never do this in production");
            Ok(Sealer::plaintext())
        }
        None => bail!("--sealing-key-file is required (create one with `beldex-lws keygen`), or pass --allow-plaintext-keys for development"),
    }
}

pub(crate) async fn open_store(args: &StoreArgs, url: &str, max_connections: u32) -> Result<Store> {
    let store = Store::connect(url, max_connections, sealer(args)?).await.context("connecting to the database")?;
    Ok(store)
}

async fn shutdown_signal(tx: watch::Sender<bool>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    info!("shutdown requested");
    let _ = tx.send(true);
}

async fn serve_http(
    name: &'static str,
    router: axum::Router,
    addr: SocketAddr,
    tls: Option<(PathBuf, PathBuf)>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let app = router.into_make_service_with_connect_info::<SocketAddr>();
    match tls {
        Some((cert, key)) => {
            let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| format!("loading TLS certificate {} / key {}", cert.display(), key.display()))?;
            let handle = axum_server::Handle::new();
            let h = handle.clone();
            tokio::spawn(async move {
                let _ = shutdown.changed().await;
                h.graceful_shutdown(Some(Duration::from_secs(20)));
            });
            info!(%addr, "{name} listening (https)");
            axum_server::bind_rustls(addr, config).handle(handle).serve(app).await?;
        }
        None => {
            let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding {addr}"))?;
            info!(%addr, "{name} listening (http)");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.changed().await;
                })
                .await?;
        }
    }
    Ok(())
}

async fn serve(mut args: ServeArgs) -> Result<()> {
    args.store.resolve()?;
    lws_kernel::self_check().context("scan kernel failed its self-check")?;
    args.validate()?;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);

    let pool_size = args.db_max_connections.unwrap_or((cores * 4).clamp(8, 64) as u32);
    let primary = Arc::new(open_store(&args.store, args.store.url(), pool_size).await?);
    if !args.no_migrate {
        primary.migrate().await.context("applying migrations")?;
    }
    primary.check_network(args.network).await?;
    // Running in the LMDB server's directory: bring its accounts over once.
    if let Some(dir) = &args.store.db_path {
        if dir.join("data.mdb").exists() && primary.counts().await?.0 == 0 {
            let summary = admin_cli::import_lmdb(&primary, args.network, dir).await?;
            info!("imported the LMDB server's accounts: {summary}");
        }
    }
    let read = match &args.database_read {
        Some(url) => Arc::new(open_store(&args.store, url, pool_size).await?),
        None => primary.clone(),
    };
    info!(backend = primary.backend(), replica = args.database_read.is_some(), network = %args.network, "database ready");

    let daemon = Arc::new(Daemon::new(
        &args.daemon_urls(),
        Duration::from_secs(args.daemon_timeout),
        Duration::from_secs(args.blocks_timeout),
    )?);
    let metrics = Arc::new(Metrics::default());
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(shutdown_signal(stop_tx));

    let mut tasks = tokio::task::JoinSet::new();

    if matches!(args.role, Role::All | Role::Scanner) {
        let threads = args.scan_threads.unwrap_or(cores);
        let config = lws_scanner::Config {
            network: args.network,
            shard: args.shard()?,
            prune: !args.no_prune,
            poll_interval: Duration::from_secs(args.poll_interval),
            lane_window: args.lane_window,
            max_lanes: args.scan_lanes,
            batches_per_pass: 20,
            fetch_window: match args.fetch_window {
                0 => (2 * daemon.endpoint_count()).clamp(2, 8),
                n => n,
            },
            accounts_per_task: 256,
            fast_prefilter: !args.no_fast_scan,
            lease_ttl: Duration::from_secs(30),
            instance: args.instance_name(),
        };
        info!(threads, shard = ?config.shard, "starting scanner");
        let scanner = lws_scanner::Scanner::new(primary.clone(), daemon.clone(), metrics.clone(), config, threads)?;
        let rx = stop_rx.clone();
        tasks.spawn(async move { scanner.run(rx).await.context("scanner") });
    }

    if matches!(args.role, Role::All | Role::Api) {
        let config = lws_api::Config {
            network: args.network,
            creation: args.account_creation,
            auto_accept_imports: args.auto_accept_imports,
            request_timeout: Duration::from_secs(args.request_timeout),
            max_in_flight: args.max_in_flight,
            rate_limit: args.rate_limit,
            rate_burst: args.rate_burst,
            creations_per_hour: args.creations_per_hour,
            trust_forwarded_for: args.trust_forwarded_for,
            cors_origins: args.cors_origin.clone(),
            response_cache_bytes: args.response_cache_mb * 1024 * 1024,
            max_response_bytes: (args.max_response_mb * 1024 * 1024) as usize,
        };
        let state = lws_api::AppState::new(config, primary.clone(), read.clone(), daemon.clone(), metrics.clone());
        let tls = args.tls_cert.clone().zip(args.tls_key.clone());
        tasks.spawn(serve_http("wallet API", lws_api::public_router(state.clone()), args.listen, tls, stop_rx.clone()));

        let token = match &args.admin_token_file {
            Some(path) => Some(read_secret_file(path)?.as_bytes().to_vec()),
            None => {
                info!("no --admin-token-file: admin routes accept only the LMDB server's admin-account auth");
                None
            }
        };
        match args.admin_addr() {
            Some(addr) => {
                let admin =
                    lws_api::admin::admin_router(Arc::new(lws_api::admin::AdminState { app: state.clone(), token }));
                tasks.spawn(serve_http("admin API", admin, addr, None, stop_rx.clone()));
            }
            None => warn!("admin API off: the wallet API uses port 8444; pass --admin-listen to enable it"),
        }
        let rx = stop_rx.clone();
        tasks.spawn(async move {
            lws_api::maintenance(state, rx).await;
            Ok(())
        });
    }

    // The first task to fail stops the rest; a clean shutdown waits for all.
    let mut result = Ok(());
    while let Some(done) = tasks.join_next().await {
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                error!(error = %format!("{e:#}"), "component failed, shutting down");
                result = Err(e);
                break;
            }
            Err(e) => {
                result = Err(e.into());
                break;
            }
        }
    }
    tasks.abort_all();
    primary.close().await;
    result
}

/// The process arguments. Invoked as `beldex-lws-daemon` (e.g. through a
/// symlink installed in place of the LMDB server), `serve` is implied, so
/// existing service files and scripts work unchanged.
fn arguments() -> Vec<String> {
    let mut args: Vec<String> = std::env::args().collect();
    let invoked_as =
        args.first().map(|a| std::path::Path::new(a).file_name().unwrap_or_default().to_string_lossy().into_owned());
    if invoked_as.as_deref() == Some("beldex-lws-daemon") {
        args.insert(1, "serve".into());
    }
    args
}

#[cfg(unix)]
extern "C" {
    fn umask(mask: u32) -> u32;
}

fn main() -> Result<()> {
    // Everything this process creates (SQLite files, keys, exports) holds
    // secrets or wallet history: owner-only, like the LMDB server's files.
    #[cfg(unix)]
    unsafe {
        umask(0o077);
    }
    settings::apply_config_file()?;
    let mut cli = Cli::parse_from(arguments());
    let mut notes = Vec::new();
    if let Command::Serve(a) = &mut cli.command {
        notes = a.apply_legacy()?;
    }
    let (log_format, log_level) = match &cli.command {
        Command::Serve(a) => (a.log_format.clone(), a.log_level.clone()),
        _ => ("text".to_owned(), "info".to_owned()),
    };
    init_logging(&log_format, &log_level);
    for note in notes {
        warn!("{note}");
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    let workers = match &cli.command {
        Command::Serve(a) => a.worker_threads,
        _ => None,
    };
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all().thread_name("lws-io");
    if let Some(n) = workers {
        runtime.worker_threads(n.max(1));
    }
    let runtime = runtime.build()?;

    runtime.block_on(async move {
        match cli.command {
            Command::Serve(args) => serve(*args).await,
            Command::Migrate(mut args) => {
                args.resolve()?;
                let store = open_store(&args, args.url(), 2).await?;
                store.migrate().await?;
                info!(backend = store.backend(), "migrations applied");
                Ok(())
            }
            Command::Admin(cmd) => admin_cli::run(cmd).await,
            Command::Keygen { out } => {
                let mut secret = zeroize::Zeroizing::new([0u8; 32]);
                rand::RngCore::fill_bytes(&mut rand::rng(), secret.as_mut());
                write_secret_file(&out, format!("{}\n", hex::encode(secret.as_ref())).as_bytes())?;
                println!("wrote {}", out.display());
                Ok(())
            }
        }
    })
}
