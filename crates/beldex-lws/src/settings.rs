//! Command-line options. Every option can also come from the environment
//! (`LWS_*`) or from a TOML file given with `--config`; precedence is
//! command line, then environment, then file.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use lws_api::Creation;
use lws_core::Network;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Role {
    /// API and scanner in one process.
    All,
    /// Wallet and admin APIs only; scale these out behind a load balancer.
    Api,
    /// Chain scanner only.
    Scanner,
}

#[derive(Args, Clone, Debug)]
pub struct StoreArgs {
    /// postgres://user:pass@host/db or sqlite:/var/lib/beldex-lws/lws.db.
    /// Prefer the environment variable: a command line is visible to every user.
    #[arg(long, env = "LWS_DATABASE_URL")]
    pub database: String,
    /// File holding the 32-byte hex key that encrypts view keys at rest.
    #[arg(long, env = "LWS_SEALING_KEY_FILE")]
    pub sealing_key_file: Option<PathBuf>,
    /// Id recorded with keys sealed under --sealing-key-file (0-255).
    #[arg(long, env = "LWS_SEALING_KEY_ID", default_value_t = 1)]
    pub sealing_key_id: u8,
    /// Retired sealing keys still accepted for reading, as ID:PATH. Repeatable.
    #[arg(long, env = "LWS_SEALING_KEY_OLD", value_delimiter = ',')]
    pub sealing_key_old: Vec<String>,
    /// Store view keys unencrypted. Development only.
    #[arg(long, env = "LWS_ALLOW_PLAINTEXT_KEYS")]
    pub allow_plaintext_keys: bool,
}

#[derive(Args, Clone, Debug)]
pub struct ServeArgs {
    #[command(flatten)]
    pub store: StoreArgs,

    /// main, test or dev. Must match the daemon and the database.
    #[arg(long, env = "LWS_NETWORK", default_value = "main")]
    pub network: Network,
    #[arg(long, env = "LWS_ROLE", value_enum, default_value_t = Role::All)]
    pub role: Role,

    /// Read-only replica for account reads (API role).
    #[arg(long, env = "LWS_DATABASE_READ_URL")]
    pub database_read: Option<String>,
    /// Connections per database pool (default: 4 per core, 8-64).
    #[arg(long, env = "LWS_DB_MAX_CONNECTIONS")]
    pub db_max_connections: Option<u32>,
    /// Skip applying migrations at startup (for replicas; run `migrate` once).
    #[arg(long, env = "LWS_NO_MIGRATE")]
    pub no_migrate: bool,

    /// beldexd RPC endpoints, primary first, comma-separated or repeated.
    #[arg(long, env = "LWS_DAEMON", value_delimiter = ',', default_value = "http://127.0.0.1:19091")]
    pub daemon: Vec<String>,
    /// Seconds before an ordinary daemon call is abandoned.
    #[arg(long, env = "LWS_DAEMON_TIMEOUT", default_value_t = 15)]
    pub daemon_timeout: u64,
    /// Seconds before a block download is abandoned.
    #[arg(long, env = "LWS_BLOCKS_TIMEOUT", default_value_t = 120)]
    pub blocks_timeout: u64,

    /// Wallet API address.
    #[arg(long, env = "LWS_LISTEN", default_value = "127.0.0.1:8443")]
    pub listen: SocketAddr,
    #[arg(long, env = "LWS_TLS_CERT", requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    #[arg(long, env = "LWS_TLS_KEY", requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// Allow plain HTTP on a non-loopback address (e.g. behind a TLS proxy).
    #[arg(long, env = "LWS_CONFIRM_EXTERNAL_BIND")]
    pub confirm_external_bind: bool,
    /// Admin and metrics address. Keep it on loopback or a private network.
    #[arg(long, env = "LWS_ADMIN_LISTEN", default_value = "127.0.0.1:8444")]
    pub admin_listen: SocketAddr,
    /// File holding the bearer token for admin routes.
    #[arg(long, env = "LWS_ADMIN_TOKEN_FILE")]
    pub admin_token_file: Option<PathBuf>,

    /// auto, manual (operator approves) or disabled.
    #[arg(long, env = "LWS_ACCOUNT_CREATION", default_value = "auto")]
    pub account_creation: Creation,
    /// Apply /import_request rescans without approval. Lets any wallet trigger
    /// a full-history scan, so leave off on public servers.
    #[arg(long, env = "LWS_AUTO_ACCEPT_IMPORTS")]
    pub auto_accept_imports: bool,
    /// Requests per second per client (0 disables).
    #[arg(long, env = "LWS_RATE_LIMIT", default_value_t = 20.0)]
    pub rate_limit: f64,
    #[arg(long, env = "LWS_RATE_BURST", default_value_t = 60.0)]
    pub rate_burst: f64,
    /// Accounts a single client may create per hour.
    #[arg(long, env = "LWS_CREATIONS_PER_HOUR", default_value_t = 30.0)]
    pub creations_per_hour: f64,
    /// Requests served concurrently per process; excess gets 503 at once.
    #[arg(long, env = "LWS_MAX_IN_FLIGHT", default_value_t = 4096)]
    pub max_in_flight: usize,
    #[arg(long, env = "LWS_REQUEST_TIMEOUT", default_value_t = 30)]
    pub request_timeout: u64,
    /// Take the client address from X-Forwarded-For. Only behind a proxy that sets it.
    #[arg(long, env = "LWS_TRUST_FORWARDED_FOR")]
    pub trust_forwarded_for: bool,
    /// Allowed CORS origins, or `*`. Repeatable.
    #[arg(long, env = "LWS_CORS_ORIGIN", value_delimiter = ',')]
    pub cors_origin: Vec<String>,
    #[arg(long, env = "LWS_RESPONSE_CACHE_MB", default_value_t = 256)]
    pub response_cache_mb: u64,
    /// Refuse responses larger than this (0 = no limit); clients paginate with max_count.
    #[arg(long, env = "LWS_MAX_RESPONSE_MB", default_value_t = 0)]
    pub max_response_mb: u64,

    /// CPU threads for scanning (default: all cores).
    #[arg(long, env = "LWS_SCAN_THREADS")]
    pub scan_threads: Option<usize>,
    /// Scan only a shard of accounts, as INDEX/COUNT (e.g. 0/4).
    #[arg(long, env = "LWS_SCAN_SHARD")]
    pub scan_shard: Option<String>,
    /// Scan lanes processed concurrently.
    #[arg(long, env = "LWS_SCAN_LANES", default_value_t = 4)]
    pub scan_lanes: usize,
    /// Accounts within this many blocks share a lane.
    #[arg(long, env = "LWS_LANE_WINDOW", default_value_t = 5000)]
    pub lane_window: u64,
    /// Seconds between polls at the chain tip.
    #[arg(long, env = "LWS_POLL_INTERVAL", default_value_t = 5)]
    pub poll_interval: u64,
    /// Run every account through the C++ kernel instead of screening with
    /// the fast prefilter first (slower; for diagnosis).
    #[arg(long, env = "LWS_NO_FAST_SCAN")]
    pub no_fast_scan: bool,
    /// Download full transactions instead of pruned ones.
    #[arg(long, env = "LWS_NO_PRUNE")]
    pub no_prune: bool,
    /// Name this process uses for scanner leases (default: host and pid).
    #[arg(long, env = "LWS_INSTANCE")]
    pub instance: Option<String>,

    /// Async I/O worker threads (default: all cores).
    #[arg(long, env = "LWS_WORKER_THREADS")]
    pub worker_threads: Option<usize>,
    #[arg(long, env = "LWS_LOG_FORMAT", default_value = "text", value_parser = ["text", "json"])]
    pub log_format: String,
    #[arg(long, env = "LWS_LOG_LEVEL", default_value = "info")]
    pub log_level: String,
}

impl ServeArgs {
    pub fn validate(&self) -> Result<()> {
        if self.tls_cert.is_none() && !self.listen.ip().is_loopback() && !self.confirm_external_bind {
            bail!(
                "refusing to serve the wallet API over plain HTTP on {}: wallets send view keys. \
                 Pass --tls-cert/--tls-key, or --confirm-external-bind if a TLS proxy sits in front",
                self.listen
            );
        }
        if !self.admin_listen.ip().is_loopback() && !self.admin_listen.ip().is_unspecified() {
            tracing::warn!(addr = %self.admin_listen, "admin API is not on loopback; make sure only operators can reach it");
        }
        if self.admin_listen.ip().is_unspecified() {
            bail!("refusing to bind the admin API to every interface ({}); pick a private address", self.admin_listen);
        }
        if self.role != Role::Scanner && self.listen == self.admin_listen {
            bail!("--listen and --admin-listen must differ");
        }
        self.shard()?;
        Ok(())
    }

    pub fn daemon_urls(&self) -> Vec<String> {
        self.daemon.iter().map(|d| d.trim().to_owned()).filter(|d| !d.is_empty()).collect()
    }

    pub fn shard(&self) -> Result<Option<(u32, u32)>> {
        let Some(spec) = &self.scan_shard else { return Ok(None) };
        let (i, n) = spec.split_once('/').context("--scan-shard takes INDEX/COUNT")?;
        let (i, n): (u32, u32) = (i.trim().parse()?, n.trim().parse()?);
        if n == 0 || i >= n {
            bail!("--scan-shard {spec}: index must be below count");
        }
        Ok(Some((i, n)))
    }

    pub fn instance_name(&self) -> String {
        self.instance.clone().unwrap_or_else(|| {
            let host =
                std::fs::read_to_string("/etc/hostname").map(|h| h.trim().to_owned()).unwrap_or_else(|_| "host".into());
            format!("{host}-{}", std::process::id())
        })
    }
}

/// Applies a TOML config file (from `--config FILE` or `LWS_CONFIG`) by
/// setting the `LWS_*` environment variable of every key that is not already
/// set, so precedence stays: command line, environment, file. Keys are the
/// long option names (`rate-limit` or `rate_limit`).
///
/// Must run before any other thread starts.
pub fn apply_config_file() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| args.iter().find_map(|a| a.strip_prefix("--config=").map(str::to_owned)))
        .or_else(|| std::env::var("LWS_CONFIG").ok());
    let Some(path) = path else { return Ok(()) };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading config file {path}"))?;
    let table: toml::Table = text.parse().with_context(|| format!("parsing config file {path}"))?;
    for (key, value) in table {
        let name = key.replace('-', "_").to_ascii_uppercase();
        let var = match name.as_str() {
            "DATABASE" => "LWS_DATABASE_URL".to_owned(),
            "DATABASE_READ" => "LWS_DATABASE_READ_URL".to_owned(),
            other => format!("LWS_{other}"),
        };
        if std::env::var_os(&var).is_some() {
            continue;
        }
        let text = match value {
            toml::Value::Array(items) => items.iter().map(plain).collect::<Vec<_>>().join(","),
            other => plain(&other),
        };
        std::env::set_var(var, text);
    }
    Ok(())
}

fn plain(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
