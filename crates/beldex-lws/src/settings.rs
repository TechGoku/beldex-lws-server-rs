//! Command-line options. Every option can also come from the environment
//! (`LWS_*`) or from a config file given with `--config` (TOML, or the LMDB
//! server's `key=value` format); precedence is command line, then
//! environment, then file. Boolean options accept `1/0`, `true/false`,
//! `yes/no` and `on/off` from the environment.
//!
//! The LMDB server's options are accepted too, with the same meaning, so an
//! existing `beldex-lws-daemon` command line or config file works unchanged
//! (see [`ServeArgs::apply_legacy`]).

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::builder::BoolishValueParser;
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
    /// Defaults to a SQLite database inside --db-path when that is given.
    #[arg(long, env = "LWS_DATABASE_URL")]
    pub database: Option<String>,
    /// The LMDB server's database directory. Without --database, this server
    /// keeps its own SQLite database and sealing key there, and on first start
    /// imports the accounts of the LMDB database it finds there.
    #[arg(long, env = "LWS_DB_PATH")]
    pub db_path: Option<PathBuf>,
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
    #[arg(long, env = "LWS_ALLOW_PLAINTEXT_KEYS", value_parser = BoolishValueParser::new())]
    pub allow_plaintext_keys: bool,
}

impl StoreArgs {
    /// Fills in defaults derived from --db-path. Creates the sealing key file
    /// there (0600) on first use.
    pub fn resolve(&mut self) -> Result<()> {
        if self.database.is_some() {
            return Ok(());
        }
        let Some(dir) = &self.db_path else {
            bail!("--database is required (or --db-path, to run in the LMDB server's directory)");
        };
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        self.database = Some(format!("sqlite:{}", dir.join("beldex-lws.sqlite").display()));
        if self.sealing_key_file.is_none() && !self.allow_plaintext_keys {
            let key = dir.join("beldex-lws-seal.key");
            if !key.exists() {
                let mut secret = zeroize::Zeroizing::new([0u8; 32]);
                rand::RngCore::fill_bytes(&mut rand::rng(), secret.as_mut());
                crate::write_secret_file(&key, format!("{}\n", hex::encode(secret.as_ref())).as_bytes())?;
                tracing::warn!(path = %key.display(), "created a sealing key next to the database; back it up, and consider moving it elsewhere (--sealing-key-file)");
            }
            self.sealing_key_file = Some(key);
        }
        Ok(())
    }

    pub fn url(&self) -> &str {
        self.database.as_deref().unwrap_or_default()
    }
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
    #[arg(long, env = "LWS_NO_MIGRATE", value_parser = BoolishValueParser::new())]
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
    #[arg(long, env = "LWS_TLS_CERT", alias = "rest-ssl-certificate")]
    pub tls_cert: Option<PathBuf>,
    #[arg(long, env = "LWS_TLS_KEY", alias = "rest-ssl-key")]
    pub tls_key: Option<PathBuf>,
    /// Allow plain HTTP on a non-loopback address (e.g. behind a TLS proxy).
    #[arg(long, env = "LWS_CONFIRM_EXTERNAL_BIND", value_parser = BoolishValueParser::new())]
    pub confirm_external_bind: bool,
    /// Admin and metrics address. Keep it on loopback or a private network.
    /// Defaults to 127.0.0.1:8444, unless the wallet API uses port 8444.
    #[arg(long, env = "LWS_ADMIN_LISTEN")]
    pub admin_listen: Option<SocketAddr>,
    /// File holding the bearer token for admin routes.
    #[arg(long, env = "LWS_ADMIN_TOKEN_FILE")]
    pub admin_token_file: Option<PathBuf>,

    /// auto, manual (operator approves) or disabled.
    #[arg(long, env = "LWS_ACCOUNT_CREATION", default_value = "auto")]
    pub account_creation: Creation,
    /// Apply /import_request rescans without approval. Lets any wallet trigger
    /// a full-history scan, so leave off on public servers.
    #[arg(long, env = "LWS_AUTO_ACCEPT_IMPORTS", value_parser = BoolishValueParser::new())]
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
    #[arg(long, env = "LWS_TRUST_FORWARDED_FOR", value_parser = BoolishValueParser::new())]
    pub trust_forwarded_for: bool,
    /// Allowed CORS origins, or `*`. Repeatable.
    #[arg(long, env = "LWS_CORS_ORIGIN", value_delimiter = ',', alias = "access-control-origin")]
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
    /// Block requests each scan lane keeps in flight, rotated across the
    /// --daemon endpoints (0 = twice the number of endpoints, at most 8).
    #[arg(long, env = "LWS_FETCH_WINDOW", default_value_t = 0)]
    pub fetch_window: usize,
    /// Accounts within this many blocks share a lane.
    #[arg(long, env = "LWS_LANE_WINDOW", default_value_t = 5000)]
    pub lane_window: u64,
    /// Seconds between polls at the chain tip.
    #[arg(long, env = "LWS_POLL_INTERVAL", default_value_t = 5)]
    pub poll_interval: u64,
    /// Run every account through the C++ kernel instead of screening with
    /// the fast prefilter first (slower; for diagnosis).
    #[arg(long, env = "LWS_NO_FAST_SCAN", value_parser = BoolishValueParser::new())]
    pub no_fast_scan: bool,
    /// Download full transactions instead of pruned ones.
    #[arg(long, env = "LWS_NO_PRUNE", value_parser = BoolishValueParser::new())]
    pub no_prune: bool,
    /// Name this process uses for scanner leases (default: host and pid).
    #[arg(long, env = "LWS_INSTANCE")]
    pub instance: Option<String>,

    /// Async I/O worker threads (default: all cores).
    #[arg(long, env = "LWS_WORKER_THREADS")]
    pub worker_threads: Option<usize>,
    #[arg(long, env = "LWS_LOG_FORMAT", default_value = "text", value_parser = ["text", "json"])]
    pub log_format: String,
    /// error, warn, info, debug, trace, a tracing filter, or the LMDB
    /// server's 0-4.
    #[arg(long, env = "LWS_LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    #[command(flatten)]
    pub legacy: LegacyServeArgs,
}

/// The LMDB server's options, for unchanged command lines and config files.
#[derive(Args, Clone, Debug, Default)]
#[command(next_help_heading = "LMDB server compatibility")]
pub struct LegacyServeArgs {
    /// Wallet API as a URL: http(s)://host:port. Overrides --listen; https
    /// needs --rest-ssl-key and --rest-ssl-certificate.
    #[arg(long)]
    pub rest_server: Vec<String>,
    /// Admin API as a URL: http://host:port. Overrides --admin-listen.
    #[arg(long)]
    pub admin_rest_server: Option<String>,
    /// More beldexd endpoints, after --daemon.
    #[arg(long, value_delimiter = ',')]
    pub daemon_backup: Vec<String>,
    /// Refuse larger responses, in bytes (see --max-response-mb).
    #[arg(long)]
    pub rest_max_response_bytes: Option<u64>,
    /// Response cache size in bytes (see --response-cache-mb).
    #[arg(long)]
    pub rest_cache_bytes: Option<u64>,
    /// Accepted for compatibility; scan threads already spread across every
    /// daemon (see --fetch-window).
    #[arg(long, value_parser = BoolishValueParser::new(), hide = true)]
    pub daemon_spread: bool,
    #[arg(long, hide = true)]
    pub db_map_size: Option<String>,
    #[arg(long, hide = true)]
    pub db_max_readers: Option<String>,
    #[arg(long, hide = true)]
    pub create_queue_max: Option<String>,
    #[arg(long, hide = true)]
    pub sub: Option<String>,
    #[arg(long, hide = true)]
    pub exchange_rate_interval: Option<String>,
    #[arg(long, hide = true)]
    pub rest_threads: Option<String>,
}

/// `http(s)://host:port[/path]` -> socket address, and whether it is https.
fn parse_rest_url(url: &str) -> Result<(SocketAddr, bool)> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let https = match scheme {
        "https" => true,
        "http" => false,
        other => bail!("{url}: unsupported scheme '{other}'"),
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let with_port = if authority.rsplit_once(':').is_some_and(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
        && !authority.ends_with(']')
    {
        authority.to_owned()
    } else {
        format!("{authority}:{}", if https { 8443 } else { 8080 })
    };
    use std::net::ToSocketAddrs;
    let addr = with_port
        .to_socket_addrs()
        .with_context(|| format!("resolving {url}"))?
        .next()
        .with_context(|| format!("{url} resolves to nothing"))?;
    Ok((addr, https))
}

impl ServeArgs {
    /// Maps the LMDB server's options onto this server's. Returns notes for
    /// options that are accepted but have no effect here.
    pub fn apply_legacy(&mut self) -> Result<Vec<String>> {
        let mut notes = Vec::new();
        let legacy = std::mem::take(&mut self.legacy);
        if let Some(first) = legacy.rest_server.first() {
            let (addr, https) = parse_rest_url(first)?;
            self.listen = addr;
            if https && (self.tls_cert.is_none() || self.tls_key.is_none()) {
                bail!("--rest-server {first} is https: pass --rest-ssl-key and --rest-ssl-certificate, or use http://");
            }
            if !https {
                self.tls_cert = None;
                self.tls_key = None;
            }
            if legacy.rest_server.len() > 1 {
                notes.push(format!("only the first --rest-server is served ({first})"));
            }
        }
        if let Some(url) = &legacy.admin_rest_server {
            let (addr, https) = parse_rest_url(url)?;
            if https {
                notes.push("the admin API is served over http; keep it on loopback or a private network".into());
            }
            self.admin_listen = Some(addr);
        }
        self.daemon.extend(legacy.daemon_backup);
        if let Some(bytes) = legacy.rest_max_response_bytes {
            self.max_response_mb = bytes.div_ceil(1024 * 1024);
        }
        if let Some(bytes) = legacy.rest_cache_bytes {
            self.response_cache_mb = (bytes / (1024 * 1024)).max(1);
        }
        for (name, set) in [
            ("--db-map-size", legacy.db_map_size.is_some()),
            ("--db-max-readers", legacy.db_max_readers.is_some()),
            ("--create-queue-max", legacy.create_queue_max.is_some()),
            ("--sub", legacy.sub.is_some()),
            ("--exchange-rate-interval", legacy.exchange_rate_interval.is_some()),
            ("--rest-threads", legacy.rest_threads.is_some()),
            ("--daemon-spread", legacy.daemon_spread),
        ] {
            if set {
                notes.push(format!("{name} is an LMDB-server option with no effect here"));
            }
        }
        self.log_level = match self.log_level.as_str() {
            "0" => "warn".into(),
            "1" => "info".into(),
            "2" => "debug".into(),
            "3" | "4" => "trace".into(),
            other => other.to_owned(),
        };
        Ok(notes)
    }

    pub fn validate(&self) -> Result<()> {
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            bail!("--tls-cert and --tls-key go together");
        }
        if self.tls_cert.is_none() && !self.listen.ip().is_loopback() && !self.confirm_external_bind {
            bail!(
                "refusing to serve the wallet API over plain HTTP on {}: wallets send view keys. \
                 Pass --tls-cert/--tls-key, or --confirm-external-bind if a TLS proxy sits in front",
                self.listen
            );
        }
        if let Some(admin) = self.admin_listen {
            if !admin.ip().is_loopback() && !admin.ip().is_unspecified() {
                tracing::warn!(addr = %admin, "admin API is not on loopback; make sure only operators can reach it");
            }
            if admin.ip().is_unspecified() {
                bail!("refusing to bind the admin API to every interface ({admin}); pick a private address");
            }
            if self.role != Role::Scanner && self.listen.port() == admin.port() {
                bail!("--listen and --admin-listen must use different ports");
            }
        }
        self.shard()?;
        Ok(())
    }

    /// Where the admin API listens, if anywhere: --admin-listen, else
    /// 127.0.0.1:8444 unless the wallet API already uses that port (as the
    /// LMDB server's examples do), in which case it stays off.
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.admin_listen.or_else(|| {
            let default: SocketAddr = "127.0.0.1:8444".parse().unwrap();
            (self.listen.port() != default.port()).then_some(default)
        })
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
        .position(|a| a == "--config" || a == "--config-file")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| {
            args.iter().find_map(|a| {
                a.strip_prefix("--config=").or_else(|| a.strip_prefix("--config-file=")).map(str::to_owned)
            })
        })
        .or_else(|| std::env::var("LWS_CONFIG").ok());
    let Some(path) = path else { return Ok(()) };
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading config file {path}"))?;
    let table: toml::Table = match text.parse() {
        Ok(table) => table,
        Err(toml_error) => parse_key_values(&text)
            .with_context(|| format!("parsing config file {path}: not TOML ({toml_error}) or key=value lines"))?,
    };
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

/// The LMDB server's config format: `key=value` per line, `#` comments,
/// repeated keys accumulate.
fn parse_key_values(text: &str) -> Result<toml::Table> {
    let mut table = toml::Table::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let (key, value) = line.split_once('=').with_context(|| format!("line {}: expected key=value", n + 1))?;
        let (key, value) = (key.trim().to_owned(), toml::Value::String(value.trim().to_owned()));
        match table.get_mut(&key) {
            Some(toml::Value::Array(items)) => items.push(value),
            Some(existing) => {
                let first = existing.clone();
                *existing = toml::Value::Array(vec![first, value]);
            }
            None => {
                table.insert(key, value);
            }
        }
    }
    Ok(table)
}

fn plain(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_urls() {
        assert_eq!(parse_rest_url("https://0.0.0.0:8443").unwrap(), ("0.0.0.0:8443".parse().unwrap(), true));
        assert_eq!(parse_rest_url("http://127.0.0.1:8091").unwrap(), ("127.0.0.1:8091".parse().unwrap(), false));
        assert_eq!(parse_rest_url("http://127.0.0.1").unwrap().0.port(), 8080);
        assert_eq!(parse_rest_url("http://[::1]:9000/admin").unwrap().0, "[::1]:9000".parse().unwrap());
        assert!(parse_rest_url("ftp://x:1").is_err());
    }

    #[test]
    fn lmdb_server_config_files_parse() {
        let t = parse_key_values("# comment\ndb-path=/var/lib/beldex-lws\ndaemon=http://a:1\ndaemon-backup=http://b:1\ndaemon-backup=http://c:1\n").unwrap();
        assert_eq!(t["db-path"].as_str(), Some("/var/lib/beldex-lws"));
        assert_eq!(t["daemon-backup"].as_array().unwrap().len(), 2);
        assert!(parse_key_values("no equals sign").is_err());
    }
}
