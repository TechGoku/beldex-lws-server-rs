//! Operations data: account search and counts for the dashboard, the audit
//! trail, and instance heartbeats. None of it reads view keys.

use super::*;

/// An account without its view key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSummary {
    pub id: AccountId,
    pub address: String,
    pub status: AccountStatus,
    pub flags: i16,
    pub start_height: u64,
    pub scan_height: u64,
    pub created_at: i64,
    pub last_access: i64,
}

#[derive(Clone, Debug, Default)]
pub struct AccountFilter {
    pub status: Option<AccountStatus>,
    /// Address prefix (or a whole address).
    pub search: Option<String>,
    /// Only accounts scanned below this height (falling behind).
    pub scanned_below: Option<u64>,
    /// Keyset pagination, newest first: ids below this one.
    pub before_id: Option<AccountId>,
    pub limit: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountCounts {
    pub active: u64,
    pub inactive: u64,
    pub hidden: u64,
    /// Active accounts scanned below the height given to `account_counts`.
    pub active_behind: u64,
    pub pending_create: u64,
    pub pending_import: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub remote: String,
    pub action: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    pub role: String,
    pub version: String,
    pub started_at: i64,
    pub heartbeat_at: i64,
    /// JSON written by the instance.
    pub stats: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub name: String,
    pub holder: String,
    pub expires_at: i64,
}

const SUMMARY_COLUMNS: &str = "id, address, status, flags, start_height, scan_height, created_at, last_access";
const MAX_PAGE: u32 = 1000;

fn summary_from_row(row: &impl Get) -> Result<AccountSummary> {
    let status = row.i16_("status")?;
    Ok(AccountSummary {
        id: row.i64_("id")?,
        address: row.text("address")?,
        status: AccountStatus::from_db(status).ok_or_else(|| Error::Corrupt(format!("account status {status}")))?,
        flags: row.i16_("flags")?,
        start_height: from_db(row.i64_("start_height")?),
        scan_height: from_db(row.i64_("scan_height")?),
        created_at: row.i64_("created_at")?,
        last_access: row.i64_("last_access")?,
    })
}

/// `prefix` with LIKE's wildcards escaped (escape character `\`).
fn like_prefix(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 1);
    for c in prefix.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

impl Store {
    /// A page of accounts, newest first.
    pub async fn account_page(&self, filter: &AccountFilter) -> Result<Vec<AccountSummary>> {
        let limit = filter.limit.clamp(1, MAX_PAGE) as i64;
        dispatch!(self, pool => {
            let mut q = QueryBuilder::new(format!("SELECT {SUMMARY_COLUMNS} FROM accounts WHERE 1 = 1"));
            if let Some(status) = filter.status {
                q.push(" AND status = ").push_bind(status.to_db());
            }
            if let Some(search) = filter.search.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                q.push(" AND address LIKE ").push_bind(like_prefix(search)).push(" ESCAPE '\\'");
            }
            if let Some(height) = filter.scanned_below {
                q.push(" AND scan_height < ").push_bind(to_db(height));
            }
            if let Some(before) = filter.before_id {
                q.push(" AND id < ").push_bind(before);
            }
            q.push(" ORDER BY id DESC LIMIT ").push_bind(limit);
            let rows = q.build().fetch_all(pool).await?;
            rows.iter().map(summary_from_row).collect()
        })
    }

    pub async fn account_summary(&self, address: &str) -> Result<Option<AccountSummary>> {
        dispatch!(self, pool => {
            let row = sqlx::query(&format!("SELECT {SUMMARY_COLUMNS} FROM accounts WHERE address = $1"))
                .bind(address).fetch_optional(pool).await?;
            row.as_ref().map(summary_from_row).transpose()
        })
    }

    /// Outputs and spends stored for one account.
    pub async fn account_activity(&self, id: AccountId) -> Result<(u64, u64)> {
        dispatch!(self, pool => {
            let o = sqlx::query("SELECT COUNT(*) AS n FROM outputs WHERE account_id = $1").bind(id).fetch_one(pool).await?.i64_("n")?;
            let s = sqlx::query("SELECT COUNT(*) AS n FROM spends WHERE account_id = $1").bind(id).fetch_one(pool).await?.i64_("n")?;
            Ok((o as u64, s as u64))
        })
    }

    /// Accounts by status, active ones behind `tip_height`, and pending
    /// requests. Served from indexes, so cheap with millions of accounts.
    pub async fn account_counts(&self, behind_height: u64) -> Result<AccountCounts> {
        let mut c = AccountCounts::default();
        dispatch!(self, pool => {
            for row in sqlx::query("SELECT status, COUNT(*) AS n FROM accounts GROUP BY status").fetch_all(pool).await? {
                let n = row.i64_("n")? as u64;
                match AccountStatus::from_db(row.i16_("status")?) {
                    Some(AccountStatus::Active) => c.active = n,
                    Some(AccountStatus::Inactive) => c.inactive = n,
                    Some(AccountStatus::Hidden) => c.hidden = n,
                    None => {}
                }
            }
            c.active_behind = sqlx::query("SELECT COUNT(*) AS n FROM accounts WHERE status = $1 AND scan_height < $2")
                .bind(AccountStatus::Active.to_db()).bind(to_db(behind_height))
                .fetch_one(pool).await?.i64_("n")? as u64;
            for row in sqlx::query("SELECT kind, COUNT(*) AS n FROM requests GROUP BY kind").fetch_all(pool).await? {
                let n = row.i64_("n")? as u64;
                match RequestKind::from_db(row.i16_("kind")?) {
                    Some(RequestKind::Create) => c.pending_create = n,
                    Some(RequestKind::ImportScan) => c.pending_import = n,
                    None => {}
                }
            }
        });
        Ok(c)
    }

    // --------------------------------------------------------------- audit

    pub async fn record_audit(&self, actor: &str, remote: &str, action: &str, detail: &str) -> Result<()> {
        dispatch!(self, pool => {
            sqlx::query("INSERT INTO audit_log (at, actor, remote, action, detail) VALUES ($1, $2, $3, $4, $5)")
                .bind(lws_core::now_secs()).bind(actor).bind(remote).bind(action).bind(detail)
                .execute(pool).await?;
        });
        Ok(())
    }

    /// Newest first; `before_id` pages back.
    pub async fn audit_log(&self, before_id: Option<i64>, limit: u32) -> Result<Vec<AuditEntry>> {
        let limit = limit.clamp(1, MAX_PAGE) as i64;
        dispatch!(self, pool => {
            let rows = sqlx::query(
                "SELECT id, at, actor, remote, action, detail FROM audit_log WHERE id < $1 ORDER BY id DESC LIMIT $2")
                .bind(before_id.unwrap_or(i64::MAX)).bind(limit).fetch_all(pool).await?;
            rows.iter().map(|r| Ok(AuditEntry {
                id: r.i64_("id")?, at: r.i64_("at")?, actor: r.text("actor")?, remote: r.text("remote")?,
                action: r.text("action")?, detail: r.text("detail")?,
            })).collect()
        })
    }

    /// Deletes audit entries older than `before` (unix seconds).
    pub async fn prune_audit(&self, before: i64) -> Result<u64> {
        dispatch!(self, pool => {
            Ok(sqlx::query("DELETE FROM audit_log WHERE at < $1").bind(before).execute(pool).await?.rows_affected())
        })
    }

    // ----------------------------------------------------------- instances

    /// Records that `name` is alive, with its current stats (JSON).
    pub async fn heartbeat(&self, name: &str, role: &str, version: &str, started_at: i64, stats: &str) -> Result<()> {
        dispatch!(self, pool => {
            sqlx::query(
                "INSERT INTO instances (name, role, version, started_at, heartbeat_at, stats) VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (name) DO UPDATE SET role = excluded.role, version = excluded.version, \
                 started_at = excluded.started_at, heartbeat_at = excluded.heartbeat_at, stats = excluded.stats")
                .bind(name).bind(role).bind(version).bind(started_at).bind(lws_core::now_secs()).bind(stats)
                .execute(pool).await?;
        });
        Ok(())
    }

    /// Instances heard from since `since` (unix seconds), by name.
    pub async fn instances(&self, since: i64) -> Result<Vec<Instance>> {
        dispatch!(self, pool => {
            let rows = sqlx::query(
                "SELECT name, role, version, started_at, heartbeat_at, stats FROM instances \
                 WHERE heartbeat_at >= $1 ORDER BY name")
                .bind(since).fetch_all(pool).await?;
            rows.iter().map(|r| Ok(Instance {
                name: r.text("name")?, role: r.text("role")?, version: r.text("version")?,
                started_at: r.i64_("started_at")?, heartbeat_at: r.i64_("heartbeat_at")?, stats: r.text("stats")?,
            })).collect()
        })
    }

    /// Forgets instances silent since before `before`, and a stopped one by name.
    pub async fn remove_instances(&self, before: i64, name: Option<&str>) -> Result<()> {
        dispatch!(self, pool => {
            sqlx::query("DELETE FROM instances WHERE heartbeat_at < $1 OR name = $2")
                .bind(before).bind(name.unwrap_or("")).execute(pool).await?;
        });
        Ok(())
    }

    pub async fn leases(&self) -> Result<Vec<Lease>> {
        dispatch!(self, pool => {
            let rows = sqlx::query("SELECT name, holder, expires_at FROM leases ORDER BY name").fetch_all(pool).await?;
            rows.iter().map(|r| Ok(Lease { name: r.text("name")?, holder: r.text("holder")?, expires_at: r.i64_("expires_at")? }))
                .collect()
        })
    }
}
