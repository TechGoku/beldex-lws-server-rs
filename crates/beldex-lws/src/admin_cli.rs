//! `beldex-lws admin ...`: account administration against the database.
//!
//! Unlike the LMDB server these commands do not take a global writer lock;
//! each runs in its own short transaction and is safe alongside a live server.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use lws_core::{account_flags, AccountStatus, Network, RequestKind, ViewKey};
use lws_store::{CreateOutcome, NewAccount, Store};
use serde::{Deserialize, Serialize};

use crate::settings::StoreArgs;
use crate::{open_store, write_secret_file};

#[derive(Args, Clone, Debug)]
pub struct Common {
    #[command(flatten)]
    pub store: StoreArgs,
    /// main, test or dev. Default: the network the database belongs to.
    #[arg(long, env = "LWS_NETWORK")]
    pub network: Option<Network>,
}

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Accounts with their status and scan heights.
    ListAccounts(Common),
    /// Pending account-creation and import requests.
    ListRequests(Common),
    /// Add an account directly. Starts at the chain tip unless --start-height is given.
    AddAccount {
        #[command(flatten)]
        common: Common,
        address: String,
        /// The account's secret view key, hex. Read from LWS_VIEW_KEY if omitted.
        #[arg(long, env = "LWS_VIEW_KEY", hide_env_values = true)]
        view_key: String,
        #[arg(long)]
        start_height: Option<u64>,
    },
    /// Approve pending requests.
    AcceptRequests {
        #[command(flatten)]
        common: Common,
        #[arg(value_parser = ["create", "import"])]
        kind: String,
        addresses: Vec<String>,
    },
    /// Discard pending requests.
    RejectRequests {
        #[command(flatten)]
        common: Common,
        #[arg(value_parser = ["create", "import"])]
        kind: String,
        addresses: Vec<String>,
    },
    /// Set accounts active, inactive (not scanned) or hidden (not scanned or served).
    SetStatus {
        #[command(flatten)]
        common: Common,
        #[arg(value_parser = ["active", "inactive", "hidden"])]
        status: String,
        addresses: Vec<String>,
    },
    /// Rescan accounts from a height. The server keeps serving meanwhile.
    Rescan {
        #[command(flatten)]
        common: Common,
        height: u64,
        addresses: Vec<String>,
    },
    /// Roll every account back to a height, as after a reorg.
    Rollback {
        #[command(flatten)]
        common: Common,
        height: u64,
        #[arg(long)]
        confirm: bool,
    },
    /// Write every account (including secret view keys) to a 0600 JSON file,
    /// in the same format as the LMDB server's `export_accounts`.
    ExportAccounts {
        #[command(flatten)]
        common: Common,
        out: PathBuf,
    },
    /// Create accounts from an export (this server's or the LMDB server's
    /// `export_accounts`); each is rebuilt from the chain from its start height.
    ImportAccounts {
        #[command(flatten)]
        common: Common,
        file: PathBuf,
    },
    /// Import accounts and pending requests straight from the LMDB server's
    /// database directory (the one holding data.mdb), read-only. Each account
    /// keeps its status, flags and start height and is rebuilt from the chain.
    ImportLmdb {
        #[command(flatten)]
        common: Common,
        /// The LMDB server's --db-path.
        db_path: PathBuf,
    },
    /// Re-encrypt every stored view key with the current sealing key.
    Reseal(Common),
}

/// The export document. Field-compatible with `beldex-lws-admin
/// export_accounts` from the LMDB implementation, so a server migrates by
/// exporting there and importing here.
#[derive(Serialize, Deserialize)]
struct ExportDoc {
    version: u32,
    #[serde(default)]
    exported_at: u64,
    network: String,
    #[serde(default)]
    source_height: u64,
    accounts: Vec<Exported>,
}

#[derive(Serialize, Deserialize)]
struct Exported {
    address: String,
    view_key: String,
    #[serde(default)]
    scan_height: u64,
    start_height: u64,
    #[serde(default = "active")]
    status: String,
    #[serde(default)]
    generated_locally: bool,
    #[serde(default)]
    admin: bool,
}

fn active() -> String {
    "active".into()
}

fn short_network(n: Network) -> &'static str {
    match n {
        Network::Mainnet => "main",
        Network::Testnet => "test",
        Network::Devnet => "dev",
    }
}

fn status_name(s: AccountStatus) -> &'static str {
    match s {
        AccountStatus::Active => "active",
        AccountStatus::Inactive => "inactive",
        AccountStatus::Hidden => "hidden",
    }
}

fn kind(name: &str) -> RequestKind {
    if name == "create" {
        RequestKind::Create
    } else {
        RequestKind::ImportScan
    }
}

/// Opens the database and works out its network: --network if given (and
/// it must match), else the one the database is bound to.
async fn open(common: &Common) -> Result<(Store, Network)> {
    let mut args = common.store.clone();
    args.resolve()?;
    let store = open_store(&args, args.url(), 2).await?;
    store.migrate().await.context("applying migrations")?;
    let network = match (common.network, store.get_meta("network").await?) {
        (Some(n), _) => n,
        (None, Some(stored)) => stored.parse().map_err(anyhow::Error::msg)?,
        (None, None) => Network::Mainnet,
    };
    store.check_network(network).await?;
    store.check_sealing_key().await?;
    Ok((store, network))
}

async fn store(common: &Common) -> Result<Store> {
    Ok(open(common).await?.0)
}

/// Records a change in the audit log, as the admin API does. The actor is
/// the local user.
async fn audit(s: &Store, action: &str, detail: serde_json::Value) {
    let user = std::env::var("SUDO_USER").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "unknown".into());
    if let Err(e) = s.record_audit(&format!("cli:{user}"), "local", action, &detail.to_string()).await {
        eprintln!("warning: could not write the audit log: {e}");
    }
}

async fn add(
    store: &Store,
    network: Network,
    address: &str,
    key: &str,
    start: u64,
    flags: i16,
    status: AccountStatus,
) -> Result<bool> {
    let (view_public, spend_public) = lws_kernel::decode_address(address, network)
        .with_context(|| format!("{address} is not a {network} address"))?;
    let view_key = ViewKey::from_hex(key).context("view key must be 64 hex characters")?;
    if !lws_kernel::view_key_matches(view_key.bytes(), &view_public) {
        bail!("view key does not belong to {address}");
    }
    let new = NewAccount {
        address: lws_kernel::encode_address(&view_public, &spend_public, network),
        view_public,
        spend_public,
        view_key,
        status,
        flags,
        start_height: start,
    };
    Ok(matches!(store.create_account(&new).await?, CreateOutcome::Created(_)))
}

/// Imports accounts and pending requests from an LMDB server database
/// directory. Returns a one-line summary.
pub async fn import_lmdb(s: &Store, network: Network, db_path: &std::path::Path) -> Result<String> {
    let old = crate::legacy::read(db_path)?;
    let (mut created, mut existing, mut invalid) = (0, 0, 0);
    for a in &old.accounts {
        if !lws_kernel::view_key_matches(&a.view_key, &a.view_public) {
            invalid += 1;
            continue;
        }
        let new = NewAccount {
            address: lws_kernel::encode_address(&a.view_public, &a.spend_public, network),
            view_public: a.view_public,
            spend_public: a.spend_public,
            view_key: ViewKey::new(*a.view_key),
            status: a.status,
            flags: i16::from(a.flags) & (account_flags::ADMIN | account_flags::GENERATED_LOCALLY),
            start_height: a.start_height,
        };
        match s.create_account(&new).await? {
            CreateOutcome::Created(_) => created += 1,
            CreateOutcome::Exists(_) => existing += 1,
        }
    }
    let mut requests = 0;
    for r in &old.requests {
        let request = lws_core::Request {
            kind: if r.kind == 0 { RequestKind::Create } else { RequestKind::ImportScan },
            address: lws_kernel::encode_address(&r.view_public, &r.spend_public, network),
            view_public: r.view_public,
            spend_public: r.spend_public,
            view_key: ViewKey::new(*r.view_key),
            start_height: r.start_height,
            flags: i16::from(r.flags) & account_flags::GENERATED_LOCALLY,
            created_at: i64::from(r.creation),
        };
        requests += usize::from(s.add_request(&request).await?);
    }
    let old_height = old.accounts.iter().map(|a| a.scan_height).max().unwrap_or(0);
    Ok(format!(
        "from {} (scanned to {old_height}): {created} account(s) imported, {existing} already present, {invalid} skipped (view key does not match); {requests} pending request(s) imported",
        db_path.display()
    ))
}

pub async fn run(cmd: AdminCommand) -> Result<()> {
    match cmd {
        AdminCommand::ListAccounts(c) => {
            let accounts = store(&c).await?.list_accounts().await?;
            let rows: Vec<_> = accounts
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "address": a.address, "status": status_name(a.status), "start_height": a.start_height,
                        "scan_height": a.scan_height, "admin": a.flags & account_flags::ADMIN != 0,
                        "created": a.created_at, "last_access": a.last_access,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        AdminCommand::ListRequests(c) => {
            let requests = store(&c).await?.list_requests().await?;
            let rows: Vec<_> = requests
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "type": if r.kind == RequestKind::Create { "create" } else { "import" },
                        "address": r.address, "start_height": r.start_height, "created": r.created_at,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&rows)?);
        }
        AdminCommand::AddAccount { common, address, view_key, start_height } => {
            let (s, network) = open(&common).await?;
            let start = match start_height {
                Some(h) => h,
                None => s.tip().await?.map(|b| b.height).unwrap_or(0),
            };
            let created = add(&s, network, &address, &view_key, start, 0, AccountStatus::Active).await?;
            if created {
                audit(&s, "add_account", serde_json::json!({"address": address, "start_height": start})).await;
            }
            println!("{}", if created { format!("added, scanning from {start}") } else { "already present".into() });
        }
        AdminCommand::AcceptRequests { common, kind: k, addresses } => {
            let s = store(&common).await?;
            let done = s.accept_requests(kind(&k), &addresses).await?;
            audit(&s, "accept_requests", serde_json::json!({"kind": k, "updated": done.len()})).await;
            println!("accepted {}", done.len());
        }
        AdminCommand::RejectRequests { common, kind: k, addresses } => {
            let s = store(&common).await?;
            let n = s.reject_requests(kind(&k), &addresses).await?.len();
            audit(&s, "reject_requests", serde_json::json!({"kind": k, "updated": n})).await;
            println!("rejected {n}");
        }
        AdminCommand::SetStatus { common, status, addresses } => {
            let status: AccountStatus = status.parse().map_err(anyhow::Error::msg)?;
            let s = store(&common).await?;
            let n = s.set_status(&addresses, status).await?.len();
            audit(&s, "set_status", serde_json::json!({"status": status_name(status), "updated": n})).await;
            println!("updated {n}");
        }
        AdminCommand::Rescan { common, height, addresses } => {
            let s = store(&common).await?;
            let n = s.rescan(&addresses, height).await?.len();
            audit(&s, "rescan", serde_json::json!({"height": height, "updated": n})).await;
            println!("rescanning {n} account(s) from {height}");
        }
        AdminCommand::Rollback { common, height, confirm } => {
            if !confirm {
                bail!("this deletes everything above {height} for every account; re-run with --confirm");
            }
            let s = store(&common).await?;
            let r = s.rollback_to(height).await?;
            audit(&s, "rollback", serde_json::json!({"height": height, "accounts": r.accounts})).await;
            println!("rolled back {} account(s): {} outputs, {} spends removed", r.accounts, r.outputs, r.spends);
        }
        AdminCommand::ExportAccounts { common, out } => {
            let (s, network) = open(&common).await?;
            let accounts = s.list_accounts().await?;
            let doc = ExportDoc {
                version: 1,
                exported_at: lws_core::now_secs() as u64,
                network: short_network(network).into(),
                source_height: s.tip().await?.map(|b| b.height).unwrap_or(0),
                accounts: accounts
                    .iter()
                    .map(|a| Exported {
                        address: a.address.clone(),
                        view_key: a.view_key.to_hex(),
                        scan_height: a.scan_height,
                        start_height: a.start_height,
                        status: status_name(a.status).into(),
                        generated_locally: a.generated_locally(),
                        admin: a.is_admin(),
                    })
                    .collect(),
            };
            let json = zeroize::Zeroizing::new(serde_json::to_vec_pretty(&doc)?);
            write_secret_file(&out, &json)?;
            audit(
                &s,
                "export_accounts",
                serde_json::json!({"accounts": doc.accounts.len(), "file": out.display().to_string()}),
            )
            .await;
            eprintln!("wrote {} account(s) to {} -- it contains secret view keys", doc.accounts.len(), out.display());
        }
        AdminCommand::ImportAccounts { common, file } => {
            let (s, network) = open(&common).await?;
            let text = zeroize::Zeroizing::new(std::fs::read_to_string(&file)?);
            let doc: ExportDoc = serde_json::from_str(&text).context("not an account export")?;
            if doc.network != short_network(network) {
                bail!("export is for network '{}', this server is '{}'", doc.network, short_network(network));
            }
            let mut created = 0;
            for r in &doc.accounts {
                let status: AccountStatus = r.status.parse().map_err(anyhow::Error::msg)?;
                let mut flags = 0;
                if r.generated_locally {
                    flags |= account_flags::GENERATED_LOCALLY;
                }
                if r.admin {
                    flags |= account_flags::ADMIN;
                }
                created += usize::from(add(&s, network, &r.address, &r.view_key, r.start_height, flags, status).await?);
            }
            audit(&s, "import_accounts", serde_json::json!({"imported": created, "file": file.display().to_string()}))
                .await;
            println!("imported {created} of {} account(s); each is rebuilt from its start height", doc.accounts.len());
        }
        AdminCommand::ImportLmdb { common, db_path } => {
            let (s, network) = open(&common).await?;
            let summary = import_lmdb(&s, network, &db_path).await?;
            audit(&s, "import_lmdb", serde_json::json!({"summary": summary, "path": db_path.display().to_string()}))
                .await;
            println!("{summary}");
        }
        AdminCommand::Reseal(c) => {
            if c.store.allow_plaintext_keys {
                bail!("reseal encrypts with the sealing key; drop --allow-plaintext-keys");
            }
            let s = store(&c).await?;
            let n = s.reseal_all().await?;
            audit(&s, "reseal", serde_json::json!({"rewritten": n, "key_id": c.store.sealing_key_id})).await;
            println!("re-sealed {n} key(s)");
        }
    }
    Ok(())
}
