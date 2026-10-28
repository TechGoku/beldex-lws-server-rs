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
    #[arg(long, env = "LWS_NETWORK", default_value = "main")]
    pub network: Network,
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

async fn store(common: &Common) -> Result<Store> {
    let store = open_store(&common.store, &common.store.database, 2).await?;
    store.check_network(common.network).await?;
    Ok(store)
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
            let s = store(&common).await?;
            let start = match start_height {
                Some(h) => h,
                None => s.tip().await?.map(|b| b.height).unwrap_or(0),
            };
            let created = add(&s, common.network, &address, &view_key, start, 0, AccountStatus::Active).await?;
            println!("{}", if created { format!("added, scanning from {start}") } else { "already present".into() });
        }
        AdminCommand::AcceptRequests { common, kind: k, addresses } => {
            let done = store(&common).await?.accept_requests(kind(&k), &addresses).await?;
            println!("accepted {}", done.len());
        }
        AdminCommand::RejectRequests { common, kind: k, addresses } => {
            let n = store(&common).await?.reject_requests(kind(&k), &addresses).await?;
            println!("rejected {n}");
        }
        AdminCommand::SetStatus { common, status, addresses } => {
            let status: AccountStatus = status.parse().map_err(anyhow::Error::msg)?;
            let n = store(&common).await?.set_status(&addresses, status).await?;
            println!("updated {n}");
        }
        AdminCommand::Rescan { common, height, addresses } => {
            let n = store(&common).await?.rescan(&addresses, height).await?;
            println!("rescanning {n} account(s) from {height}");
        }
        AdminCommand::Rollback { common, height, confirm } => {
            if !confirm {
                bail!("this deletes everything above {height} for every account; re-run with --confirm");
            }
            let r = store(&common).await?.rollback_to(height).await?;
            println!("rolled back {} account(s): {} outputs, {} spends removed", r.accounts, r.outputs, r.spends);
        }
        AdminCommand::ExportAccounts { common, out } => {
            let s = store(&common).await?;
            let accounts = s.list_accounts().await?;
            let doc = ExportDoc {
                version: 1,
                exported_at: lws_core::now_secs() as u64,
                network: short_network(common.network).into(),
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
            eprintln!("wrote {} account(s) to {} -- it contains secret view keys", doc.accounts.len(), out.display());
        }
        AdminCommand::ImportAccounts { common, file } => {
            let s = store(&common).await?;
            let text = zeroize::Zeroizing::new(std::fs::read_to_string(&file)?);
            let doc: ExportDoc = serde_json::from_str(&text).context("not an account export")?;
            if doc.network != short_network(common.network) {
                bail!("export is for network '{}', this server is '{}'", doc.network, short_network(common.network));
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
                created +=
                    usize::from(add(&s, common.network, &r.address, &r.view_key, r.start_height, flags, status).await?);
            }
            println!("imported {created} of {} account(s); each is rebuilt from its start height", doc.accounts.len());
        }
        AdminCommand::Reseal(c) => {
            if c.store.sealing_key_file.is_none() {
                bail!("reseal needs --sealing-key-file (the key to seal with)");
            }
            let n = store(&c).await?.reseal_all().await?;
            println!("re-sealed {n} key(s)");
        }
    }
    Ok(())
}
