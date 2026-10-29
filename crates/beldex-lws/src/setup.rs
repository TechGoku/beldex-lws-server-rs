//! `beldex-lws setup` writes a working configuration in one step, and
//! `beldex-lws doctor` checks an installation end to end.
//!
//! Setup finds beldexd on this machine (and with it the network), creates the
//! data directory, the sealing key and the admin token, and writes a commented
//! config file. Asked on a terminal, it confirms each choice; with `--yes`, or
//! when not on a terminal, it takes the flags and the defaults.

use std::io::{BufRead, IsTerminal, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Args;
use lws_core::Network;
use lws_daemon::Daemon;
use lws_store::{Sealer, Store};

use crate::settings::{
    self, default_daemon, is_root, write_new_key, ServeArgs, ADMIN_TOKEN_FILE, SEALING_KEY_FILE, SQLITE_FILE,
};

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Config file to write. Default: /etc/beldex-lws/lws.toml as root,
    /// else lws.toml in the data directory.
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long, env = "LWS_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// main, test or dev. Default: whatever the local beldexd runs.
    #[arg(long)]
    pub network: Option<Network>,
    /// beldexd RPC endpoints. Default: the beldexd found on this machine.
    #[arg(long, value_delimiter = ',')]
    pub daemon: Vec<String>,
    /// postgres://user:pass@host/db. Default: SQLite in the data directory.
    #[arg(long)]
    pub database: Option<String>,
    /// Wallets reach this server through a TLS proxy (Caddy, nginx) on this
    /// machine; the server trusts the proxy's X-Forwarded-For.
    #[arg(long)]
    pub behind_proxy: bool,
    /// Serve HTTPS directly with this certificate (PEM; needs --tls-key).
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// Wallet API address. Default: 127.0.0.1:8443, or 0.0.0.0:8443 with TLS.
    #[arg(long)]
    pub listen: Option<SocketAddr>,
    /// auto, manual or disabled.
    #[arg(long, default_value = "auto")]
    pub account_creation: String,
    /// Ask nothing: use the flags and defaults.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Replace an existing config file.
    #[arg(long)]
    pub force: bool,
    /// Set up in a web browser instead of the terminal.
    #[arg(long)]
    pub web: bool,
    /// Where the browser setup listens (loopback only).
    #[arg(long, default_value = "127.0.0.1:8445")]
    pub web_listen: SocketAddr,
}

struct Prompt {
    interactive: bool,
}

impl Prompt {
    fn ask(&self, question: &str, default: &str) -> Result<String> {
        if !self.interactive {
            return Ok(default.to_owned());
        }
        if default.is_empty() {
            eprint!("{question}: ");
        } else {
            eprint!("{question} [{default}]: ");
        }
        std::io::stderr().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let line = line.trim();
        Ok(if line.is_empty() { default.to_owned() } else { line.to_owned() })
    }
}

/// get_info from one endpoint, with a short timeout.
pub async fn probe_daemon(url: &str) -> lws_daemon::Result<lws_daemon::Info> {
    Daemon::new(&[url.to_owned()], Duration::from_secs(4), Duration::from_secs(4))?.get_info().await
}

/// The first daemon among `urls` (or the default ports) that answers, and
/// the network it runs.
pub async fn find_daemon(urls: &[String], network: Option<Network>) -> Option<(String, Network, lws_daemon::Info)> {
    let candidates: Vec<String> = if !urls.is_empty() {
        urls.to_vec()
    } else {
        match network {
            Some(n) => vec![default_daemon(n)],
            None => [Network::Mainnet, Network::Testnet, Network::Devnet].map(default_daemon).to_vec(),
        }
    };
    for url in candidates {
        if let Ok(info) = probe_daemon(&url).await {
            if let Ok(n) = info.nettype.parse::<Network>() {
                return Some((url, n, info));
            }
        }
    }
    None
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

pub async fn setup(args: SetupArgs) -> Result<()> {
    if args.web {
        return crate::setup_web::run(args).await;
    }
    lws_kernel::self_check().context("the scan kernel failed its self-check; reinstall the server")?;
    let prompt = Prompt { interactive: !args.yes && std::io::stdin().is_terminal() };
    if prompt.interactive {
        eprintln!("Setting up the Beldex light wallet server. Press Enter to accept a [default].\n");
    }

    // beldexd, and from it the network.
    let mut urls = args.daemon.clone();
    let mut typed = false;
    let (daemons, network) = loop {
        let found = find_daemon(&urls, args.network).await;
        let list = match (&found, urls.is_empty()) {
            (_, false) => urls.clone(),
            (Some((url, ..)), true) => vec![url.clone()],
            (None, true) => vec![default_daemon(args.network.unwrap_or(Network::Mainnet))],
        };
        let current = list.join(",");
        match &found {
            Some((url, n, info)) => eprintln!("Found beldexd at {url}: {}, height {}.", n.daemon_name(), info.height),
            None => eprintln!("No beldexd answered at {current}."),
        }
        // An address the user just typed is taken once it answers.
        if !(typed && found.is_some()) {
            let answer = prompt.ask("beldexd RPC address (several: comma-separated)", &current)?.replace(' ', "");
            if answer != current {
                urls = answer.split(',').filter(|u| !u.is_empty()).map(str::to_owned).collect();
                typed = true;
                continue;
            }
        }
        let network = match found {
            Some((url, n, _)) => {
                if let Some(wanted) = args.network.filter(|w| *w != n) {
                    bail!("beldexd at {url} runs {}, not {}", n.daemon_name(), wanted.daemon_name());
                }
                n
            }
            // Never guess the network: a wrong one is only caught at the first start.
            None => {
                let n = match args.network {
                    Some(n) => n,
                    None if !prompt.interactive => {
                        bail!("no beldexd answered at {current}; start it, or pass --network to configure without it")
                    }
                    None => loop {
                        match prompt
                            .ask("beldexd did not answer. Which network does it run? (mainnet, testnet, devnet)", "")?
                            .parse()
                        {
                            Ok(n) => break n,
                            Err(e) => eprintln!("{e}"),
                        }
                    },
                };
                eprintln!(
                    "warning: beldexd did not answer; configuring {}. Start beldexd, then run `beldex-lws doctor`.",
                    n.daemon_name()
                );
                n
            }
        };
        break (list, network);
    };

    // Database.
    let database = match &args.database {
        Some(url) => Some(url.clone()),
        None => {
            let answer = prompt
                .ask("Database: Enter for SQLite (one machine), or a postgres:// URL (several servers)", "sqlite")?;
            (answer != "sqlite").then_some(answer)
        }
    };
    if let Some(url) = &database {
        check_database(url).await?;
        eprintln!("PostgreSQL is reachable.");
    }

    // How wallets reach the server.
    let (listen, tls, behind_proxy) = if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        (args.listen.unwrap_or_else(|| "0.0.0.0:8443".parse().unwrap()), Some((cert.clone(), key.clone())), false)
    } else if args.behind_proxy {
        (args.listen.unwrap_or_else(|| "127.0.0.1:8443".parse().unwrap()), None, true)
    } else if args.listen.is_some() || !prompt.interactive {
        (args.listen.unwrap_or_else(|| "127.0.0.1:8443".parse().unwrap()), None, false)
    } else {
        eprintln!("\nHow will wallets reach this server?");
        eprintln!("  1) only from this machine (or a VPN/SSH tunnel)");
        eprintln!("  2) from the internet, through a TLS proxy on this machine (Caddy, nginx)");
        eprintln!("  3) from the internet, with a TLS certificate served by beldex-lws itself");
        match prompt.ask("Choice", "1")?.as_str() {
            "2" => ("127.0.0.1:8443".parse().unwrap(), None, true),
            "3" => {
                let cert = PathBuf::from(prompt.ask("Certificate chain (PEM)", "/etc/beldex-lws/tls/fullchain.pem")?);
                let key = PathBuf::from(prompt.ask("Private key (PEM)", "/etc/beldex-lws/tls/privkey.pem")?);
                ("0.0.0.0:8443".parse().unwrap(), Some((cert, key)), false)
            }
            _ => ("127.0.0.1:8443".parse().unwrap(), None, false),
        }
    };
    let data_dir = args.data_dir.clone().unwrap_or_else(settings::default_data_dir);
    let plan = Plan {
        network,
        daemons,
        database,
        listen,
        tls,
        behind_proxy,
        account_creation: args.account_creation.clone(),
        out: args.out.clone().unwrap_or_else(|| default_config_path(&data_dir)),
        data_dir,
        force: args.force,
    };
    check_plan(&plan).await?;
    let written = write_plan(&plan)?;

    eprintln!();
    eprintln!("Wrote {}", written.out.display());
    for path in &written.created {
        eprintln!("Created {}", path.display());
    }
    eprintln!("  network   {}", plan.network.daemon_name());
    eprintln!("  beldexd   {}", plan.daemons.join(", "));
    eprintln!(
        "  database  {}",
        if plan.database.is_some() {
            "PostgreSQL".to_owned()
        } else {
            format!("SQLite in {}", plan.data_dir.display())
        }
    );
    eprintln!("  wallets   {}://{}", if plan.tls.is_some() { "https" } else { "http" }, plan.listen);
    eprintln!("  admin     http://127.0.0.1:8444 (dashboard; token in {})", written.token.display());
    if plan.behind_proxy {
        eprintln!("\nPoint the proxy at {}. With Caddy, the whole Caddyfile is:\n", plan.listen);
        eprintln!("    lws.example.com {{\n        reverse_proxy {}\n    }}\n", plan.listen);
    }
    eprintln!(
        "\nBack up {} together with the database: without it, view keys cannot be read.",
        written.seal_key.display()
    );
    let out = written.out.display();
    eprintln!("\nNext:\n  beldex-lws doctor --config {out}\n  beldex-lws serve --config {out}");
    Ok(())
}

/// Everything setup decides; the terminal and web front ends both build one.
#[derive(Clone, Debug)]
pub struct Plan {
    pub network: Network,
    pub daemons: Vec<String>,
    /// PostgreSQL URL; None for SQLite in the data directory.
    pub database: Option<String>,
    pub listen: SocketAddr,
    pub tls: Option<(PathBuf, PathBuf)>,
    pub behind_proxy: bool,
    pub account_creation: String,
    pub data_dir: PathBuf,
    pub out: PathBuf,
    pub force: bool,
}

pub struct Written {
    pub out: PathBuf,
    pub created: Vec<PathBuf>,
    pub seal_key: PathBuf,
    pub token: PathBuf,
    pub text: String,
}

/// /etc/beldex-lws/lws.toml as root, else lws.toml in the data directory.
pub fn default_config_path(data_dir: &Path) -> PathBuf {
    if is_root() {
        PathBuf::from("/etc/beldex-lws/lws.toml")
    } else {
        data_dir.join("lws.toml")
    }
}

pub async fn check_database(url: &str) -> Result<()> {
    if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
        bail!("expected a postgres:// URL");
    }
    Store::probe(url, std::time::Duration::from_secs(8)).await.map_err(|e| match e {
        lws_store::Error::Db(sqlx_error) if sqlx_error.to_string().contains("pool timed out") => {
            anyhow::anyhow!("no answer within 8 seconds (host or firewall?)")
        }
        other => anyhow::anyhow!("{other}"),
    })
}

pub async fn check_tls(cert: &Path, key: &Path) -> Result<()> {
    axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key)
        .await
        .with_context(|| format!("loading TLS certificate {} / key {}", cert.display(), key.display()))?;
    Ok(())
}

/// Everything that can be checked before writing anything.
pub async fn check_plan(plan: &Plan) -> Result<()> {
    if plan.daemons.is_empty() {
        bail!("at least one beldexd is needed");
    }
    if !["auto", "manual", "disabled"].contains(&plan.account_creation.as_str()) {
        bail!("account creation must be auto, manual or disabled");
    }
    if let Some((cert, key)) = &plan.tls {
        check_tls(cert, key).await?;
    } else if !plan.listen.ip().is_loopback() {
        bail!("wallets on a public address need TLS: give a certificate, or keep the API on loopback behind a proxy");
    }
    if plan.out.exists() && !plan.force {
        bail!("{} already exists; replace it explicitly", plan.out.display());
    }
    Ok(())
}

/// Creates the data directory, keys (only if missing) and the config file.
pub fn write_plan(plan: &Plan) -> Result<Written> {
    let data_dir = &plan.data_dir;
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let seal_key = data_dir.join(SEALING_KEY_FILE);
    let token = data_dir.join(ADMIN_TOKEN_FILE);
    let mut created = Vec::new();
    for path in [&seal_key, &token] {
        if !path.exists() {
            write_new_key(path)?;
            created.push(path.clone());
        }
    }

    let mut text = String::new();
    text.push_str("# beldex-lws configuration, written by `beldex-lws setup`.\n");
    text.push_str(
        "# Keys are the long options of `beldex-lws serve --help`; LWS_* variables and flags override them.\n",
    );
    text.push_str("# Check it with: beldex-lws doctor --config <this file>\n\n");
    text.push_str(&format!("network = {}\n", toml_str(&plan.network.to_string())));
    text.push_str(&format!("data-dir = {}\n", toml_str(&data_dir.display().to_string())));
    match &plan.database {
        Some(url) => text.push_str(&format!(
            "database = {}\n# Every server sharing this database needs this same key: copy it, never regenerate it.\n",
            toml_str(url)
        )),
        None => text.push_str(&format!("# Database: SQLite at {}.\n", data_dir.join(SQLITE_FILE).display())),
    }
    text.push_str(&format!("sealing-key-file = {}\n", toml_str(&seal_key.display().to_string())));
    text.push_str(&format!("admin-token-file = {}\n\n", toml_str(&token.display().to_string())));
    let quoted: Vec<String> = plan.daemons.iter().map(|d| toml_str(d)).collect();
    text.push_str(&format!("daemon = [{}]\n", quoted.join(", ")));
    match settings::detect_notify_socket(plan.network) {
        Some(sock) => text.push_str(&format!("# New blocks are announced over {sock} (found automatically).\n")),
        None => text.push_str(
            "# notify = [\"ipc:///var/lib/beldex/beldexd.sock\"]   # beldexd's socket: new blocks are seen at once\n",
        ),
    }
    text.push('\n');
    text.push_str(&format!("listen = {}\n", toml_str(&plan.listen.to_string())));
    if let Some((cert, key)) = &plan.tls {
        text.push_str(&format!("tls-cert = {}\n", toml_str(&cert.display().to_string())));
        text.push_str(&format!("tls-key = {}\n", toml_str(&key.display().to_string())));
    }
    if plan.behind_proxy {
        text.push_str("trust-forwarded-for = true                  # the proxy on this machine sets X-Forwarded-For\n");
    }
    text.push_str("admin-listen = \"127.0.0.1:8444\"             # dashboard and admin API\n");
    text.push_str(&format!("account-creation = {}\n", toml_str(&plan.account_creation)));
    text.push_str("\n# Tuning (defaults shown):\n");
    text.push_str("# rate-limit = 20           # requests/second per client\n");
    text.push_str("# max-in-flight = 4096\n");
    text.push_str("# scan-threads = <all cores>\n");
    text.push_str("# audit-retention-days = 365\n");
    text.push_str("# log-format = \"text\"       # or \"json\"\n");

    if let Some(dir) = plan.out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    write_config(&plan.out, &text, plan.force)?;
    Ok(Written { out: plan.out.clone(), created, seal_key, token, text })
}

fn write_config(path: &Path, text: &str, replace: bool) -> Result<()> {
    if replace && path.exists() {
        std::fs::remove_file(path).with_context(|| format!("replacing {}", path.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

// ------------------------------------------------------------------ doctor

#[derive(Default)]
struct Report {
    failures: usize,
    warnings: usize,
}

impl Report {
    fn line(&mut self, mark: &str, topic: &str, detail: impl std::fmt::Display) {
        println!("{mark:<5} {topic:<14} {detail}");
    }
    fn ok(&mut self, topic: &str, detail: impl std::fmt::Display) {
        self.line("ok", topic, detail);
    }
    fn warn(&mut self, topic: &str, detail: impl std::fmt::Display) {
        self.warnings += 1;
        self.line("WARN", topic, detail);
    }
    fn fail(&mut self, topic: &str, detail: impl std::fmt::Display) {
        self.failures += 1;
        self.line("FAIL", topic, detail);
    }
}

fn check_secret(report: &mut Report, topic: &str, path: &Path, missing: &str) -> bool {
    if !path.exists() {
        report.ok(topic, format!("{} ({missing})", path.display()));
        return false;
    }
    match crate::read_secret_file(path) {
        Ok(text) if lws_core::parse_key(&text).is_some() => {
            report.ok(topic, path.display());
            true
        }
        Ok(_) => {
            report.fail(topic, format!("{} must hold 64 hex characters", path.display()));
            false
        }
        Err(e) => {
            report.fail(topic, format!("{e:#}"));
            false
        }
    }
}

/// Checks configuration, files, database, daemons, notifications, TLS and
/// ports, printing one line per check. Changes nothing, except that a
/// database from before the sealing check gets its check value recorded.
pub async fn doctor(mut args: ServeArgs) -> Result<()> {
    let mut r = Report::default();
    println!("beldex-lws {} doctor, network {}\n", env!("CARGO_PKG_VERSION"), args.network.daemon_name());

    match lws_kernel::self_check() {
        Ok(()) => r.ok("scan kernel", "self-check passed"),
        Err(e) => r.fail("scan kernel", format!("{e:#}")),
    }
    match args.validate() {
        Ok(()) => r.ok("config", "options are consistent"),
        Err(e) => r.fail("config", format!("{e:#}")),
    }

    // Files, without creating anything.
    let dir = args.store.dir();
    r.ok("data dir", dir.display());
    let url = args.store.database.clone().unwrap_or_else(|| format!("sqlite:{}", dir.join(SQLITE_FILE).display()));
    let key_path = args.store.sealing_key_file.clone().unwrap_or_else(|| dir.join(SEALING_KEY_FILE));
    let have_key = if args.store.allow_plaintext_keys && args.store.sealing_key_file.is_none() {
        r.warn("sealing key", "none: view keys are stored unencrypted (--allow-plaintext-keys)");
        false
    } else if !key_path.exists() && !url.starts_with("sqlite:") {
        r.fail("sealing key", format!("{} is missing; PostgreSQL needs --sealing-key-file", key_path.display()));
        false
    } else {
        check_secret(&mut r, "sealing key", &key_path, "created on first start")
    };
    if matches!(args.role, settings::Role::All | settings::Role::Api) {
        let token = args.admin_token_file.clone().unwrap_or_else(|| dir.join(ADMIN_TOKEN_FILE));
        check_secret(&mut r, "admin token", &token, "created on first start");
    }

    // Database.
    let sqlite_file = url.strip_prefix("sqlite:").map(|p| PathBuf::from(p.split('?').next().unwrap_or(p)));
    let mut stored_tip = None;
    if sqlite_file.as_ref().is_some_and(|f| !f.exists()) {
        r.ok("database", format!("{url} (created on first start)"));
    } else {
        args.store.database = Some(url.clone());
        if have_key {
            args.store.sealing_key_file = Some(key_path.clone());
        }
        let sealer = if have_key { crate::sealer(&args.store) } else { Ok(Sealer::plaintext()) };
        match sealer {
            Err(e) => r.fail("sealing key", format!("{e:#}")),
            Ok(sealer) => match Store::connect(&url, 2, sealer).await {
                Err(e) => r.fail("database", format!("{e}")),
                Ok(store) => {
                    let backend = store.backend();
                    match store.get_meta("network").await {
                        Err(_) => r.ok("database", format!("{backend}, empty (set up on first start)")),
                        Ok(None) => r.ok("database", format!("{backend}, not bound to a network yet")),
                        Ok(Some(n)) if n != args.network.daemon_name() => r.fail(
                            "database",
                            format!("belongs to {n}, but --network is {}", args.network.daemon_name()),
                        ),
                        Ok(Some(_)) => {
                            match store.counts().await {
                                Ok((accounts, outputs, _)) => {
                                    r.ok("database", format!("{backend}, {accounts} account(s), {outputs} output(s)"))
                                }
                                Err(e) => r.fail("database", e),
                            }
                            match store.check_sealing_key().await {
                                Ok(()) => r.ok("sealing key", "opens the stored view keys"),
                                Err(e) => r.fail("sealing key", e),
                            }
                            if store.counts().await.is_ok_and(|(accounts, ..)| accounts > 0) {
                                stored_tip = store.tip().await.ok().flatten().map(|b| b.height);
                            }
                        }
                    }
                    store.close().await;
                }
            },
        }
    }

    // Daemons.
    let mut best = 0;
    let mut heights = Vec::new();
    for url in args.daemon_urls() {
        match probe_daemon(&url).await {
            Err(lws_daemon::Error::Unavailable(_)) => {
                r.fail("beldexd", format!("{url}: no answer (is beldexd running, with this RPC port?)"))
            }
            Err(e) => r.fail("beldexd", format!("{url}: {e}")),
            Ok(info) if info.nettype != args.network.daemon_name() => {
                r.fail("beldexd", format!("{url} runs {}, not {}", info.nettype, args.network.daemon_name()))
            }
            Ok(info) => {
                best = best.max(info.height);
                heights.push((url.clone(), info.height));
                if info.target_height > info.height + 5 {
                    r.warn("beldexd", format!("{url}: still syncing ({} of {})", info.height, info.target_height));
                } else if info.outgoing_connections + info.incoming_connections == 0 {
                    r.warn("beldexd", format!("{url}: height {}, but no peers", info.height));
                } else {
                    r.ok("beldexd", format!("{url}: height {}", info.height));
                }
            }
        }
    }
    for (url, h) in &heights {
        if best > h + 5 {
            r.warn("beldexd", format!("{url} is {} blocks behind the others", best - h));
        }
    }
    if let (Some(tip), true) = (stored_tip, best > 0) {
        let behind = best.saturating_sub(tip + 1);
        let detail = format!("chain stored to {tip}, {behind} block(s) behind beldexd");
        if behind > 100 {
            r.warn("scanner", detail);
        } else {
            r.ok("scanner", detail);
        }
    }

    // Block notifications.
    let (endpoints, auto) = args.notify_endpoints();
    if endpoints.is_empty() {
        let hint = settings::local_notify_sockets(args.network)
            .into_iter()
            .find(|p| p.exists())
            .and_then(|p| std::os::unix::net::UnixStream::connect(&p).err().map(|e| format!("; {}: {e}", p.display())))
            .unwrap_or_default();
        r.warn(
            "notifications",
            format!("off: new blocks are seen at the next poll (every {}s){hint}", args.poll_interval),
        );
    }
    for endpoint in endpoints {
        let e = endpoint.clone();
        match tokio::task::spawn_blocking(move || lws_daemon::notify::probe(&e, Duration::from_secs(4))).await? {
            Ok(()) => r.ok("notifications", format!("{endpoint}{}", if auto { " (found automatically)" } else { "" })),
            Err(e) => r.fail("notifications", format!("{endpoint}: {e}")),
        }
    }

    // Listeners.
    if matches!(args.role, settings::Role::All | settings::Role::Api) {
        match (&args.tls_cert, &args.tls_key) {
            (Some(cert), Some(key)) => match axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await {
                Ok(_) => r.ok("tls", format!("{} loads", cert.display())),
                Err(e) => r.fail("tls", format!("{}: {e}", cert.display())),
            },
            _ if args.listen.ip().is_loopback() => {
                r.ok("tls", "none; wallet API on loopback (put a TLS proxy in front for remote wallets)")
            }
            _ => r.warn("tls", "none; plain HTTP on a public address (only behind a TLS proxy)"),
        }
        let admin = args.admin_addr();
        let running = match admin {
            Some(addr) => running_server(addr).await,
            None => None,
        };
        for (name, addr) in [("wallet API", Some(args.listen)), ("admin API", admin)] {
            let Some(addr) = addr else {
                r.warn(name, "off");
                continue;
            };
            match std::net::TcpListener::bind(addr) {
                Ok(_) => r.ok(name, format!("{addr} is free")),
                Err(_) if running.is_some() => r.ok(name, format!("{addr} (beldex-lws is running)")),
                Err(e) => r.fail(name, format!("{addr}: {e} (another program holds it?)")),
            }
        }
        if let Some(health) = running {
            let scanner = &health["scanner"];
            r.ok(
                "running",
                format!(
                    "status {}, {} account(s) scanning, lowest scan height {}",
                    health["status"].as_str().unwrap_or("?"),
                    scanner["accounts"],
                    scanner["min_scan_height"]
                ),
            );
        }
    }

    println!();
    match (r.failures, r.warnings) {
        (0, 0) => println!("Everything looks good."),
        (0, w) => println!("No problems, {w} warning(s)."),
        (f, _) => bail!("{f} problem(s) found"),
    }
    Ok(())
}

/// The /health document of a beldex-lws already listening on `admin`.
async fn running_server(admin: SocketAddr) -> Option<serde_json::Value> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(3)).build().ok()?;
    let body: serde_json::Value = client.get(format!("http://{admin}/health")).send().await.ok()?.json().await.ok()?;
    body.get("status")?;
    Some(body)
}
