//! Storage for the light wallet server: PostgreSQL for production, SQLite for
//! development and single-machine installs.
//!
//! Every method is written once and run against either backend through
//! [`dispatch!`], so the two cannot drift apart in behaviour. Correctness rules
//! the rest of the server relies on:
//!
//! * A scanner commit writes outputs, spends, block hashes and the new scan
//!   height in one transaction, so a reader never sees a scan height whose
//!   outputs are missing.
//! * A commit is checked against the chain epoch and each account's generation;
//!   anything fetched before a reorg or rescan is rejected rather than written.
//! * A scan height only moves backwards through [`Store::rollback_to`] (reorg)
//!   or [`Store::rescan`] (operator), both of which also delete the rows above it.

use std::str::FromStr;
use std::time::Duration;

use lws_core::{
    from_db, to_db, Account, AccountId, AccountStatus, BlockId, Key, Network, Output, OutputRef, Request, RequestKind,
    Spend, TokenData, ViewKey,
};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{QueryBuilder, Row};

mod sealing;
pub use ops::{AccountCounts, AccountFilter, AccountSummary, AuditEntry, Instance, Lease};
pub use sealing::Sealer;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("corrupt row: {0}")]
    Corrupt(String),
    #[error("database belongs to {stored}, but the server is configured for {configured}")]
    NetworkMismatch { stored: String, configured: String },
    #[error("a reorg was handled while the batch was in flight")]
    EpochChanged,
    #[error("stored block hash at height {0} disagrees with the batch")]
    ChainConflict(u64),
    #[error("unsupported database URL '{0}' (expected postgres://... or sqlite:...)")]
    UnsupportedUrl(String),
    #[error("{0}")]
    WrongSealingKey(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The pool had no free connection in time: a capacity problem, not a bug.
    pub fn is_overload(&self) -> bool {
        matches!(self, Error::Db(sqlx::Error::PoolTimedOut))
    }
}

enum Pool {
    Postgres(PgPool),
    Sqlite(SqlitePool),
}

/// Runs `$body` with `$pool` bound to whichever backend is configured. The body
/// is compiled once per backend, so every query is checked against both.
macro_rules! dispatch {
    ($self:expr, $pool:ident => $body:expr) => {
        match &$self.pool {
            Pool::Postgres($pool) => $body,
            Pool::Sqlite($pool) => $body,
        }
    };
}

/// Typed column access shared by both row types.
trait Get {
    fn i64_(&self, col: &str) -> Result<i64>;
    fn i32_(&self, col: &str) -> Result<i32>;
    fn i16_(&self, col: &str) -> Result<i16>;
    fn opt_i64(&self, col: &str) -> Result<Option<i64>>;
    fn bytes(&self, col: &str) -> Result<Vec<u8>>;
    fn opt_bytes(&self, col: &str) -> Result<Option<Vec<u8>>>;
    fn text(&self, col: &str) -> Result<String>;
}

macro_rules! impl_get {
    ($row:ty) => {
        impl Get for $row {
            fn i64_(&self, col: &str) -> Result<i64> {
                Ok(self.try_get::<i64, _>(col)?)
            }
            fn i32_(&self, col: &str) -> Result<i32> {
                Ok(self.try_get::<i32, _>(col)?)
            }
            fn i16_(&self, col: &str) -> Result<i16> {
                Ok(self.try_get::<i16, _>(col)?)
            }
            fn opt_i64(&self, col: &str) -> Result<Option<i64>> {
                Ok(self.try_get::<Option<i64>, _>(col)?)
            }
            fn bytes(&self, col: &str) -> Result<Vec<u8>> {
                Ok(self.try_get::<Vec<u8>, _>(col)?)
            }
            fn opt_bytes(&self, col: &str) -> Result<Option<Vec<u8>>> {
                Ok(self.try_get::<Option<Vec<u8>>, _>(col)?)
            }
            fn text(&self, col: &str) -> Result<String> {
                Ok(self.try_get::<String, _>(col)?)
            }
        }
    };
}
impl_get!(PgRow);
impl_get!(SqliteRow);

fn key(bytes: Vec<u8>, what: &str) -> Result<Key> {
    Key::try_from(bytes.as_slice()).map_err(|_| Error::Corrupt(format!("{what} is {} bytes, expected 32", bytes.len())))
}

fn opt_key(bytes: Option<Vec<u8>>, what: &str) -> Result<Option<Key>> {
    bytes.map(|b| key(b, what)).transpose()
}

const ACCOUNT_COLUMNS: &str = "id, address, view_public, spend_public, view_key, status, flags, \
     start_height, scan_height, generation, created_at, last_access";

fn account_from_row(sealer: &Sealer, row: &impl Get) -> Result<Account> {
    let status = row.i16_("status")?;
    let address = row.text("address")?;
    Ok(Account {
        id: row.i64_("id")?,
        view_key: sealer.open(&address, &row.bytes("view_key")?)?,
        address,
        view_public: key(row.bytes("view_public")?, "view_public")?,
        spend_public: key(row.bytes("spend_public")?, "spend_public")?,
        status: AccountStatus::from_db(status).ok_or_else(|| Error::Corrupt(format!("account status {status}")))?,
        flags: row.i16_("flags")?,
        start_height: from_db(row.i64_("start_height")?),
        scan_height: from_db(row.i64_("scan_height")?),
        generation: row.i64_("generation")?,
        created_at: row.i64_("created_at")?,
        last_access: row.i64_("last_access")?,
    })
}

const OUTPUT_COLUMNS: &str = "account_id, ring_amount, global_index, height, tx_hash, tx_prefix_hash, tx_pub_key, \
     out_index, amount, mask, pub_key, unlock_time, timestamp, mixin, flags, payment_id, locked_key_image, \
     token_id, blinded_token_id, amount_commitment, encrypted_amount, rct";

fn output_from_row(row: &impl Get) -> Result<Output> {
    let token = match opt_key(row.opt_bytes("token_id")?, "token_id")? {
        None => None,
        Some(token_id) => Some(TokenData {
            token_id,
            blinded_token_id: opt_key(row.opt_bytes("blinded_token_id")?, "blinded_token_id")?.unwrap_or_default(),
            amount_commitment: opt_key(row.opt_bytes("amount_commitment")?, "amount_commitment")?.unwrap_or_default(),
            encrypted_amount: row.opt_i64("encrypted_amount")?.map(from_db).unwrap_or(0),
        }),
    };
    Ok(Output {
        account_id: row.i64_("account_id")?,
        height: from_db(row.i64_("height")?),
        tx_hash: key(row.bytes("tx_hash")?, "tx_hash")?,
        tx_prefix_hash: key(row.bytes("tx_prefix_hash")?, "tx_prefix_hash")?,
        tx_pub_key: key(row.bytes("tx_pub_key")?, "tx_pub_key")?,
        out_index: row.i32_("out_index")? as u32,
        source: OutputRef {
            amount: from_db(row.i64_("ring_amount")?),
            global_index: from_db(row.i64_("global_index")?),
        },
        amount: from_db(row.i64_("amount")?),
        mask: key(row.bytes("mask")?, "mask")?,
        pub_key: key(row.bytes("pub_key")?, "pub_key")?,
        unlock_time: from_db(row.i64_("unlock_time")?),
        timestamp: from_db(row.i64_("timestamp")?),
        mixin: row.i32_("mixin")? as u32,
        flags: row.i16_("flags")?,
        payment_id: row.opt_bytes("payment_id")?,
        locked_key_image: opt_key(row.opt_bytes("locked_key_image")?, "locked_key_image")?,
        token,
        rct: row.opt_bytes("rct")?,
    })
}

const SPEND_COLUMNS: &str =
    "account_id, key_image, source_amount, source_index, height, tx_hash, timestamp, unlock_time, mixin, payment_id";

fn spend_from_row(row: &impl Get) -> Result<Spend> {
    Ok(Spend {
        account_id: row.i64_("account_id")?,
        height: from_db(row.i64_("height")?),
        tx_hash: key(row.bytes("tx_hash")?, "tx_hash")?,
        key_image: key(row.bytes("key_image")?, "key_image")?,
        source: OutputRef {
            amount: from_db(row.i64_("source_amount")?),
            global_index: from_db(row.i64_("source_index")?),
        },
        timestamp: from_db(row.i64_("timestamp")?),
        unlock_time: from_db(row.i64_("unlock_time")?),
        mixin: row.i32_("mixin")? as u32,
        payment_id: row.opt_bytes("payment_id")?,
    })
}

fn request_from_row(sealer: &Sealer, row: &impl Get) -> Result<Request> {
    let kind = row.i16_("kind")?;
    let address = row.text("address")?;
    Ok(Request {
        kind: RequestKind::from_db(kind).ok_or_else(|| Error::Corrupt(format!("request kind {kind}")))?,
        view_key: sealer.open(&address, &row.bytes("view_key")?)?,
        address,
        view_public: key(row.bytes("view_public")?, "view_public")?,
        spend_public: key(row.bytes("spend_public")?, "spend_public")?,
        start_height: from_db(row.i64_("start_height")?),
        flags: row.i16_("flags")?,
        created_at: row.i64_("created_at")?,
    })
}

/// Rows per multi-row INSERT; keeps every statement under SQLite's and
/// PostgreSQL's bind-parameter limits.
const INSERT_CHUNK: usize = 400;
/// lws_meta key (and sealing associated data) of the sealing-key check value.
const SEAL_CHECK: &str = "seal_check";

#[derive(Clone, Debug)]
pub struct NewAccount {
    pub address: String,
    pub view_public: Key,
    pub spend_public: Key,
    pub view_key: ViewKey,
    pub status: AccountStatus,
    pub flags: i16,
    pub start_height: u64,
}

#[derive(Debug)]
pub enum CreateOutcome {
    Created(Account),
    Exists(Account),
}

/// One account's progress in a scanner commit.
#[derive(Clone, Copy, Debug)]
pub struct AccountProgress {
    pub id: AccountId,
    pub generation: i64,
    /// The scan height the scanner started from; the commit is refused if the
    /// stored height moved in the meantime.
    pub from_height: u64,
    pub to_height: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ScanCommit {
    pub epoch: i64,
    pub blocks: Vec<BlockId>,
    pub accounts: Vec<AccountProgress>,
    pub outputs: Vec<Output>,
    pub spends: Vec<Spend>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommitReport {
    pub accepted: Vec<AccountId>,
    pub rejected: Vec<AccountId>,
    pub outputs: u64,
    pub spends: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RollbackReport {
    pub accounts: u64,
    pub outputs: u64,
    pub spends: u64,
}

pub struct Store {
    pool: Pool,
    sealer: Sealer,
}

impl Store {
    /// Opens `url`: `postgres://user:pass@host/db` or `sqlite:/path/to/lws.db`
    /// (the file is created if missing).
    pub async fn connect(url: &str, max_connections: u32, sealer: Sealer) -> Result<Store> {
        let pool = if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let options = PgConnectOptions::from_str(url)?.application_name("beldex-lws");
            Pool::Postgres(
                PgPoolOptions::new()
                    .max_connections(max_connections.max(2))
                    .acquire_timeout(Duration::from_secs(10))
                    .connect_with(options)
                    .await?,
            )
        } else if url.starts_with("sqlite:") {
            let options = SqliteConnectOptions::from_str(url)?
                .create_if_missing(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .foreign_keys(true)
                .busy_timeout(Duration::from_secs(30));
            Pool::Sqlite(
                SqlitePoolOptions::new()
                    .max_connections(max_connections.clamp(1, 16))
                    .acquire_timeout(Duration::from_secs(30))
                    .connect_with(options)
                    .await?,
            )
        } else {
            return Err(Error::UnsupportedUrl(url.to_owned()));
        };
        Ok(Store { pool, sealer })
    }

    /// Opens one connection to `url` and closes it, reporting the real cause
    /// of a failure (refused, unknown host, authentication, missing
    /// database) rather than a pool timeout. For setup and diagnostics.
    pub async fn probe(url: &str, timeout: Duration) -> Result<()> {
        use sqlx::Connection;
        if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
            return Err(Error::UnsupportedUrl(url.to_owned()));
        }
        let options = PgConnectOptions::from_str(url)?.application_name("beldex-lws-setup");
        let conn = tokio::time::timeout(timeout, sqlx::PgConnection::connect_with(&options))
            .await
            .map_err(|_| Error::Db(sqlx::Error::PoolTimedOut))??;
        conn.close().await?;
        Ok(())
    }

    pub fn backend(&self) -> &'static str {
        match self.pool {
            Pool::Postgres(_) => "postgres",
            Pool::Sqlite(_) => "sqlite",
        }
    }

    /// Applies pending migrations. Each runs once, inside a transaction, and is
    /// recorded; a checksum mismatch on an applied migration is an error.
    pub async fn migrate(&self) -> Result<()> {
        match &self.pool {
            Pool::Postgres(pool) => sqlx::migrate!("./migrations/postgres").run(pool).await?,
            Pool::Sqlite(pool) => sqlx::migrate!("./migrations/sqlite").run(pool).await?,
        }
        Ok(())
    }

    pub async fn ping(&self) -> Result<()> {
        dispatch!(self, pool => { sqlx::query("SELECT 1").execute(pool).await?; });
        Ok(())
    }

    /// Runs one statement outside any transaction. For tooling and tests
    /// (e.g. `CREATE DATABASE`); never pass untrusted input.
    pub async fn execute_admin_sql(&self, sql: &str) -> Result<()> {
        dispatch!(self, pool => { sqlx::raw_sql(sql).execute(pool).await?; });
        Ok(())
    }

    pub async fn close(&self) {
        dispatch!(self, pool => pool.close().await)
    }

    /// Writes a consistent copy of a SQLite database to `path`, which must
    /// not exist or be empty, while the database stays in use. Returns false
    /// for PostgreSQL, which is backed up with its own tools.
    pub async fn sqlite_backup(&self, path: &str) -> Result<bool> {
        match &self.pool {
            Pool::Sqlite(pool) => {
                sqlx::query("VACUUM INTO $1").bind(path).execute(pool).await?;
                Ok(true)
            }
            Pool::Postgres(_) => Ok(false),
        }
    }

    // ---------------------------------------------------------------- meta

    pub async fn get_meta(&self, key: &str) -> Result<Option<String>> {
        dispatch!(self, pool => {
            let row = sqlx::query("SELECT value FROM lws_meta WHERE key = $1").bind(key).fetch_optional(pool).await?;
            row.map(|r| r.text("value")).transpose()
        })
    }

    pub async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        dispatch!(self, pool => {
            sqlx::query("INSERT INTO lws_meta (key, value) VALUES ($1, $2) \
                         ON CONFLICT (key) DO UPDATE SET value = excluded.value")
                .bind(key).bind(value).execute(pool).await?;
        });
        Ok(())
    }

    /// Binds the database to a network on first use and refuses any other
    /// afterwards, so a testnet daemon can never roll back mainnet accounts.
    pub async fn check_network(&self, network: Network) -> Result<()> {
        let configured = network.daemon_name().to_owned();
        match self.get_meta("network").await? {
            None => self.set_meta("network", &configured).await,
            Some(stored) if stored == configured => Ok(()),
            Some(stored) => Err(Error::NetworkMismatch { stored, configured }),
        }
    }

    /// Refuses a sealing key that cannot open what the database holds, so a
    /// server started with the wrong key (another machine's, or none) fails
    /// at startup instead of on every login. The first start records a check
    /// value sealed with the current key; `reseal_all` renews it.
    pub async fn check_sealing_key(&self) -> Result<()> {
        let wrong = |why: &str| {
            Error::WrongSealingKey(format!(
                "{why} (wrong --sealing-key-file? While rotating keys, pass the old one with --sealing-key-old)"
            ))
        };
        const MISMATCH: &str = "the sealing key cannot open this database's view keys";
        let check = self.get_meta(SEAL_CHECK).await?;
        if let Some(check) = &check {
            let stored = hex::decode(check).map_err(|_| Error::Corrupt("sealing check value".into()))?;
            if stored.len() != 32 {
                return match self.sealer.open(SEAL_CHECK, &stored) {
                    Ok(_) => Ok(()),
                    Err(_) if !self.sealer.is_encrypting() => {
                        Err(wrong("this database's view keys are encrypted, but no sealing key is configured"))
                    }
                    Err(_) => Err(wrong(MISMATCH)),
                };
            }
            // Recorded while keys were stored in plaintext: proves nothing
            // about a key, so verify a stored key below and upgrade it.
        }
        // No usable check value (an older database): try one stored key.
        let sample = dispatch!(self, pool => {
            let row = sqlx::query("SELECT address, view_key FROM accounts ORDER BY id DESC LIMIT 1").fetch_optional(pool).await?;
            row.map(|r| Ok::<_, Error>((r.text("address")?, r.bytes("view_key")?))).transpose()?
        });
        if let Some((address, stored)) = sample {
            self.sealer.open(&address, &stored).map_err(|_| wrong(MISMATCH))?;
        }
        if check.is_none() || self.sealer.is_encrypting() {
            self.write_seal_check().await?;
        }
        Ok(())
    }

    async fn write_seal_check(&self) -> Result<()> {
        let sealed = self.sealer.seal(SEAL_CHECK, &ViewKey::new([0; 32]))?;
        self.set_meta(SEAL_CHECK, &hex::encode(sealed)).await
    }

    // --------------------------------------------------------------- chain

    pub async fn chain_epoch(&self) -> Result<i64> {
        dispatch!(self, pool => {
            let row = sqlx::query("SELECT epoch FROM chain_state WHERE id = 1").fetch_one(pool).await?;
            row.i64_("epoch")
        })
    }

    pub async fn tip(&self) -> Result<Option<BlockId>> {
        dispatch!(self, pool => {
            let row = sqlx::query("SELECT height, hash FROM blocks ORDER BY height DESC LIMIT 1").fetch_optional(pool).await?;
            row.map(|r| Ok(BlockId { height: from_db(r.i64_("height")?), hash: key(r.bytes("hash")?, "block hash")? })).transpose()
        })
    }

    pub async fn block_hash(&self, height: u64) -> Result<Option<Key>> {
        dispatch!(self, pool => {
            let row = sqlx::query("SELECT hash FROM blocks WHERE height = $1").bind(to_db(height)).fetch_optional(pool).await?;
            row.map(|r| key(r.bytes("hash")?, "block hash")).transpose()
        })
    }

    /// Stores block hashes the scanner did not otherwise record (the genesis
    /// block at startup). Existing hashes are left alone.
    pub async fn insert_blocks(&self, blocks: &[BlockId]) -> Result<()> {
        dispatch!(self, pool => {
            for chunk in blocks.chunks(INSERT_CHUNK) {
                let mut q = QueryBuilder::new("INSERT INTO blocks (height, hash) ");
                q.push_values(chunk, |mut b, block| {
                    b.push_bind(to_db(block.height)).push_bind(block.hash.to_vec());
                });
                q.push(" ON CONFLICT (height) DO NOTHING");
                q.build().execute(pool).await?;
            }
        });
        Ok(())
    }

    /// The block ids a `get_blocks.bin` request carries so the daemon can find
    /// where its chain and ours split: `top` and the nine below it, then
    /// exponentially further back, then genesis. Highest first; heights that
    /// were never stored are skipped.
    pub async fn chain_sample(&self, top: u64) -> Result<Vec<BlockId>> {
        let mut heights = Vec::new();
        let mut step = 1u64;
        let mut h = top;
        loop {
            heights.push(h);
            if heights.len() >= 10 {
                step = step.saturating_mul(2);
            }
            if h == 0 {
                break;
            }
            h = h.saturating_sub(step);
        }
        let mut found = dispatch!(self, pool => {
            let mut q = QueryBuilder::new("SELECT height, hash FROM blocks WHERE height IN (");
            let mut sep = q.separated(", ");
            for h in &heights {
                sep.push_bind(to_db(*h));
            }
            q.push(")");
            let rows = q.build().fetch_all(pool).await?;
            rows.iter()
                .map(|r| Ok(BlockId { height: from_db(r.i64_("height")?), hash: key(r.bytes("hash")?, "block hash")? }))
                .collect::<Result<Vec<_>>>()?
        });
        found.sort_by_key(|b| std::cmp::Reverse(b.height));
        Ok(found)
    }

    /// Reorg: forgets everything above `height`. Accounts scanned past it are
    /// moved back to it (and their generation bumped), their outputs and spends
    /// above it deleted, and the chain epoch advanced so in-flight batches from
    /// the abandoned branch are refused at commit.
    pub async fn rollback_to(&self, height: u64) -> Result<RollbackReport> {
        let h = to_db(height);
        dispatch!(self, pool => {
            let mut tx = pool.begin().await?;
            sqlx::query("UPDATE chain_state SET epoch = epoch + 1 WHERE id = 1").execute(&mut *tx).await?;
            sqlx::query("DELETE FROM blocks WHERE height > $1").bind(h).execute(&mut *tx).await?;
            let outputs = sqlx::query("DELETE FROM outputs WHERE height > $1").bind(h).execute(&mut *tx).await?.rows_affected();
            let spends = sqlx::query("DELETE FROM spends WHERE height > $1").bind(h).execute(&mut *tx).await?.rows_affected();
            let accounts = sqlx::query(
                "UPDATE accounts SET scan_height = $1, generation = generation + 1 WHERE scan_height > $1")
                .bind(h).execute(&mut *tx).await?.rows_affected();
            tx.commit().await?;
            Ok(RollbackReport { accounts, outputs, spends })
        })
    }

    // ------------------------------------------------------------ accounts

    pub async fn account_by_address(&self, address: &str) -> Result<Option<Account>> {
        dispatch!(self, pool => {
            let row = sqlx::query(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE address = $1"))
                .bind(address).fetch_optional(pool).await?;
            row.map(|r| account_from_row(&self.sealer, &r)).transpose()
        })
    }

    pub async fn account_by_id(&self, id: AccountId) -> Result<Option<Account>> {
        dispatch!(self, pool => {
            let row = sqlx::query(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = $1"))
                .bind(id).fetch_optional(pool).await?;
            row.map(|r| account_from_row(&self.sealer, &r)).transpose()
        })
    }

    pub async fn list_accounts(&self) -> Result<Vec<Account>> {
        dispatch!(self, pool => {
            let rows = sqlx::query(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY id")).fetch_all(pool).await?;
            rows.iter().map(|r| account_from_row(&self.sealer, r)).collect()
        })
    }

    pub async fn admin_accounts(&self) -> Result<Vec<Account>> {
        dispatch!(self, pool => {
            let rows = sqlx::query(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE (flags & 1) = 1 ORDER BY id"))
                .fetch_all(pool).await?;
            rows.iter().map(|r| account_from_row(&self.sealer, r)).collect()
        })
    }

    /// Active accounts for the scanner. With `shard = Some((index, count))`
    /// only accounts whose id falls in that shard are returned. Accounts last
    /// used before `used_since` are idle: left out until a wallet uses them
    /// again, when they resume from their scan height and catch up.
    pub async fn scan_targets(&self, shard: Option<(u32, u32)>, used_since: Option<i64>) -> Result<Vec<Account>> {
        let (index, count) = shard.unwrap_or((0, 1));
        dispatch!(self, pool => {
            let rows = sqlx::query(&format!(
                "SELECT {ACCOUNT_COLUMNS} FROM accounts \
                 WHERE status = $1 AND (id % $2) = $3 AND last_access >= $4 ORDER BY scan_height, id"))
                .bind(AccountStatus::Active.to_db())
                .bind(i64::from(count.max(1)))
                .bind(i64::from(index))
                .bind(used_since.unwrap_or(i64::MIN))
                .fetch_all(pool).await?;
            rows.iter().map(|r| account_from_row(&self.sealer, r)).collect()
        })
    }

    pub async fn create_account(&self, new: &NewAccount) -> Result<CreateOutcome> {
        let now = lws_core::now_secs();
        let inserted = dispatch!(self, pool => {
            let row = sqlx::query(&format!(
                "INSERT INTO accounts (address, view_public, spend_public, view_key, status, flags, start_height, \
                 scan_height, generation, created_at, last_access) VALUES ($1, $2, $3, $4, $5, $6, $7, $7, 0, $8, $8) \
                 ON CONFLICT (address) DO NOTHING RETURNING {ACCOUNT_COLUMNS}"))
                .bind(&new.address)
                .bind(new.view_public.to_vec())
                .bind(new.spend_public.to_vec())
                .bind(self.sealer.seal(&new.address, &new.view_key)?)
                .bind(new.status.to_db())
                .bind(new.flags)
                .bind(to_db(new.start_height))
                .bind(now)
                .fetch_optional(pool).await?;
            row.map(|r| account_from_row(&self.sealer, &r)).transpose()?
        });
        match inserted {
            Some(account) => Ok(CreateOutcome::Created(account)),
            None => self
                .account_by_address(&new.address)
                .await?
                .map(CreateOutcome::Exists)
                .ok_or_else(|| Error::Corrupt(format!("account {} vanished during insert", new.address))),
        }
    }

    /// Returns the addresses that exist (and so were updated).
    pub async fn set_status(&self, addresses: &[String], status: AccountStatus) -> Result<Vec<String>> {
        let mut changed = Vec::new();
        dispatch!(self, pool => {
            let mut tx = pool.begin().await?;
            for address in addresses {
                if sqlx::query("UPDATE accounts SET status = $1 WHERE address = $2")
                    .bind(status.to_db()).bind(address).execute(&mut *tx).await?.rows_affected() == 1 {
                    changed.push(address.clone());
                }
            }
            tx.commit().await?;
        });
        Ok(changed)
    }

    pub async fn set_flags(&self, address: &str, flags: i16) -> Result<u64> {
        dispatch!(self, pool => {
            Ok(sqlx::query("UPDATE accounts SET flags = $1 WHERE address = $2")
                .bind(flags).bind(address).execute(pool).await?.rows_affected())
        })
    }

    /// Operator rescan: moves the accounts' start and scan height to `height`
    /// and deletes what was found above it, in one transaction.
    /// Returns the addresses that exist (and so were rescanned).
    pub async fn rescan(&self, addresses: &[String], height: u64) -> Result<Vec<String>> {
        let h = to_db(height);
        let mut changed = Vec::new();
        dispatch!(self, pool => {
            let mut tx = pool.begin().await?;
            for address in addresses {
                let row = sqlx::query("SELECT id FROM accounts WHERE address = $1").bind(address).fetch_optional(&mut *tx).await?;
                let Some(row) = row else { continue };
                let id = row.i64_("id")?;
                sqlx::query("DELETE FROM outputs WHERE account_id = $1 AND height > $2").bind(id).bind(h).execute(&mut *tx).await?;
                sqlx::query("DELETE FROM spends WHERE account_id = $1 AND height > $2").bind(id).bind(h).execute(&mut *tx).await?;
                sqlx::query(
                    "UPDATE accounts SET start_height = $1, scan_height = $1, generation = generation + 1 WHERE id = $2")
                    .bind(h).bind(id).execute(&mut *tx).await?;
                changed.push(address.clone());
            }
            tx.commit().await?;
        });
        Ok(changed)
    }

    /// Records that the account was used, at most once a minute per account so
    /// balance polling does not turn every read into a write.
    pub async fn touch(&self, id: AccountId, now: i64) -> Result<()> {
        dispatch!(self, pool => {
            sqlx::query("UPDATE accounts SET last_access = $1 WHERE id = $2 AND last_access < $3")
                .bind(now).bind(id).bind(now - 60).execute(pool).await?;
        });
        Ok(())
    }

    /// Re-seals every stored view key with the current sealing key (key
    /// rotation, or encrypting a database that was run without one).
    /// Returns the number of keys rewritten.
    pub async fn reseal_all(&self) -> Result<u64> {
        let accounts = self.list_accounts().await?;
        let requests = self.list_requests().await?;
        let mut rewritten = 0;
        dispatch!(self, pool => {
            let mut tx = pool.begin().await?;
            for a in &accounts {
                rewritten += sqlx::query("UPDATE accounts SET view_key = $1 WHERE id = $2")
                    .bind(self.sealer.seal(&a.address, &a.view_key)?)
                    .bind(a.id)
                    .execute(&mut *tx).await?.rows_affected();
            }
            for r in &requests {
                rewritten += sqlx::query("UPDATE requests SET view_key = $1 WHERE kind = $2 AND address = $3")
                    .bind(self.sealer.seal(&r.address, &r.view_key)?)
                    .bind(r.kind.to_db())
                    .bind(&r.address)
                    .execute(&mut *tx).await?.rows_affected();
            }
            tx.commit().await?;
        });
        self.write_seal_check().await?;
        Ok(rewritten)
    }

    // ------------------------------------------------------------ requests

    /// Returns false when an identical request is already pending.
    pub async fn add_request(&self, request: &Request) -> Result<bool> {
        dispatch!(self, pool => {
            let added = sqlx::query(
                "INSERT INTO requests (kind, address, view_public, spend_public, view_key, start_height, flags, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (kind, address) DO NOTHING")
                .bind(request.kind.to_db())
                .bind(&request.address)
                .bind(request.view_public.to_vec())
                .bind(request.spend_public.to_vec())
                .bind(self.sealer.seal(&request.address, &request.view_key)?)
                .bind(to_db(request.start_height))
                .bind(request.flags)
                .bind(request.created_at)
                .execute(pool).await?.rows_affected();
            Ok(added == 1)
        })
    }

    pub async fn get_request(&self, kind: RequestKind, address: &str) -> Result<Option<Request>> {
        dispatch!(self, pool => {
            let row = sqlx::query("SELECT kind, address, view_public, spend_public, view_key, start_height, flags, created_at FROM requests \
                                   WHERE kind = $1 AND address = $2")
                .bind(kind.to_db()).bind(address).fetch_optional(pool).await?;
            row.map(|r| request_from_row(&self.sealer, &r)).transpose()
        })
    }

    pub async fn list_requests(&self) -> Result<Vec<Request>> {
        dispatch!(self, pool => {
            let rows = sqlx::query("SELECT kind, address, view_public, spend_public, view_key, start_height, flags, created_at FROM requests \
                                    ORDER BY created_at, address").fetch_all(pool).await?;
            rows.iter().map(|r| request_from_row(&self.sealer, r)).collect()
        })
    }

    /// Returns the addresses whose request was pending (and so removed).
    pub async fn reject_requests(&self, kind: RequestKind, addresses: &[String]) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        dispatch!(self, pool => {
            let mut tx = pool.begin().await?;
            for address in addresses {
                if sqlx::query("DELETE FROM requests WHERE kind = $1 AND address = $2")
                    .bind(kind.to_db()).bind(address).execute(&mut *tx).await?.rows_affected() == 1 {
                    removed.push(address.clone());
                }
            }
            tx.commit().await?;
        });
        Ok(removed)
    }

    /// Approves pending requests. A create request becomes an account; an
    /// import request rescans the existing account from its requested height.
    /// Returns the addresses that were acted on.
    pub async fn accept_requests(&self, kind: RequestKind, addresses: &[String]) -> Result<Vec<String>> {
        let mut accepted = Vec::new();
        for address in addresses {
            let Some(request) = self.get_request(kind, address).await? else { continue };
            match kind {
                RequestKind::Create => {
                    let new = NewAccount {
                        address: address.clone(),
                        view_public: request.view_public,
                        spend_public: request.spend_public,
                        view_key: request.view_key.clone(),
                        status: AccountStatus::Active,
                        flags: request.flags,
                        start_height: request.start_height,
                    };
                    self.create_account(&new).await?;
                }
                RequestKind::ImportScan => {
                    self.rescan(std::slice::from_ref(address), request.start_height).await?;
                }
            }
            self.reject_requests(kind, std::slice::from_ref(address)).await?;
            accepted.push(address.clone());
        }
        Ok(accepted)
    }

    // ------------------------------------------------------------ scanning

    /// Which accounts own any of `refs`. Used to find candidate spends: a
    /// batch's ring members are looked up here instead of holding every
    /// account's outputs in memory, so scanner memory does not grow with the
    /// number of accounts.
    pub async fn owners_of(&self, refs: &[OutputRef]) -> Result<Vec<(AccountId, OutputRef)>> {
        let mut out = Vec::new();
        dispatch!(self, pool => {
            for chunk in refs.chunks(4 * INSERT_CHUNK) {
                let mut q = QueryBuilder::new("SELECT account_id, ring_amount, global_index FROM outputs WHERE global_index IN (");
                let mut sep = q.separated(", ");
                for r in chunk {
                    sep.push_bind(to_db(r.global_index));
                }
                q.push(")");
                for row in q.build().fetch_all(pool).await? {
                    let found = OutputRef { amount: from_db(row.i64_("ring_amount")?), global_index: from_db(row.i64_("global_index")?) };
                    if chunk.contains(&found) {
                        out.push((row.i64_("account_id")?, found));
                    }
                }
            }
        });
        Ok(out)
    }

    /// Every output reference each account owns, for matching ring members.
    pub async fn owned_refs(&self, account: AccountId) -> Result<Vec<OutputRef>> {
        dispatch!(self, pool => {
            let rows = sqlx::query("SELECT ring_amount, global_index FROM outputs WHERE account_id = $1")
                .bind(account).fetch_all(pool).await?;
            rows.iter()
                .map(|r| Ok(OutputRef { amount: from_db(r.i64_("ring_amount")?), global_index: from_db(r.i64_("global_index")?) }))
                .collect()
        })
    }

    /// Writes one scanned batch atomically. Refused with `ChainConflict` when a
    /// reorg landed since the batch was fetched or a stored block hash
    /// disagrees with it; individual accounts that were rescanned, rolled back
    /// or advanced by someone else meanwhile are skipped (`rejected`).
    pub async fn commit_scan(&self, commit: &ScanCommit) -> Result<CommitReport> {
        dispatch!(self, pool => {
            let mut tx = pool.begin().await?;

            // Locks chain_state for the rest of the transaction; rollbacks take
            // the same lock, so the two are serialised.
            let locked = sqlx::query("UPDATE chain_state SET epoch = epoch WHERE id = 1 AND epoch = $1")
                .bind(commit.epoch).execute(&mut *tx).await?.rows_affected();
            if locked != 1 {
                return Err(Error::EpochChanged);
            }

            for chunk in commit.blocks.chunks(INSERT_CHUNK) {
                let mut q = QueryBuilder::new("SELECT height, hash FROM blocks WHERE height IN (");
                let mut sep = q.separated(", ");
                for b in chunk {
                    sep.push_bind(to_db(b.height));
                }
                q.push(")");
                for row in q.build().fetch_all(&mut *tx).await? {
                    let height = from_db(row.i64_("height")?);
                    let stored = key(row.bytes("hash")?, "block hash")?;
                    if chunk.iter().any(|b| b.height == height && b.hash != stored) {
                        return Err(Error::ChainConflict(height));
                    }
                }
                let mut q = QueryBuilder::new("INSERT INTO blocks (height, hash) ");
                q.push_values(chunk, |mut b, block| {
                    b.push_bind(to_db(block.height)).push_bind(block.hash.to_vec());
                });
                q.push(" ON CONFLICT (height) DO NOTHING");
                q.build().execute(&mut *tx).await?;
            }

            let mut report = CommitReport::default();
            for progress in &commit.accounts {
                let moved = sqlx::query(
                    "UPDATE accounts SET scan_height = $1 WHERE id = $2 AND generation = $3 AND scan_height = $4")
                    .bind(to_db(progress.to_height))
                    .bind(progress.id)
                    .bind(progress.generation)
                    .bind(to_db(progress.from_height))
                    .execute(&mut *tx).await?.rows_affected();
                if moved == 1 {
                    report.accepted.push(progress.id);
                } else {
                    report.rejected.push(progress.id);
                }
            }

            let outputs: Vec<&Output> = commit.outputs.iter().filter(|o| report.accepted.contains(&o.account_id)).collect();
            for chunk in outputs.chunks(INSERT_CHUNK) {
                let mut q = QueryBuilder::new(format!("INSERT INTO outputs ({OUTPUT_COLUMNS}) "));
                q.push_values(chunk, |mut b, o| {
                    let token = o.token.as_ref();
                    b.push_bind(o.account_id)
                        .push_bind(to_db(o.source.amount))
                        .push_bind(to_db(o.source.global_index))
                        .push_bind(to_db(o.height))
                        .push_bind(o.tx_hash.to_vec())
                        .push_bind(o.tx_prefix_hash.to_vec())
                        .push_bind(o.tx_pub_key.to_vec())
                        .push_bind(o.out_index as i32)
                        .push_bind(to_db(o.amount))
                        .push_bind(o.mask.to_vec())
                        .push_bind(o.pub_key.to_vec())
                        .push_bind(to_db(o.unlock_time))
                        .push_bind(to_db(o.timestamp))
                        .push_bind(o.mixin as i32)
                        .push_bind(o.flags)
                        .push_bind(o.payment_id.clone())
                        .push_bind(o.locked_key_image.map(|k| k.to_vec()))
                        .push_bind(token.map(|t| t.token_id.to_vec()))
                        .push_bind(token.map(|t| t.blinded_token_id.to_vec()))
                        .push_bind(token.map(|t| t.amount_commitment.to_vec()))
                        .push_bind(token.map(|t| to_db(t.encrypted_amount)))
                        .push_bind(o.rct.clone());
                });
                q.push(" ON CONFLICT DO NOTHING");
                report.outputs += q.build().execute(&mut *tx).await?.rows_affected();
            }

            let spends: Vec<&Spend> = commit.spends.iter().filter(|s| report.accepted.contains(&s.account_id)).collect();
            for chunk in spends.chunks(INSERT_CHUNK) {
                let mut q = QueryBuilder::new(format!("INSERT INTO spends ({SPEND_COLUMNS}) "));
                q.push_values(chunk, |mut b, s| {
                    b.push_bind(s.account_id)
                        .push_bind(s.key_image.to_vec())
                        .push_bind(to_db(s.source.amount))
                        .push_bind(to_db(s.source.global_index))
                        .push_bind(to_db(s.height))
                        .push_bind(s.tx_hash.to_vec())
                        .push_bind(to_db(s.timestamp))
                        .push_bind(to_db(s.unlock_time))
                        .push_bind(s.mixin as i32)
                        .push_bind(s.payment_id.clone());
                });
                q.push(" ON CONFLICT DO NOTHING");
                report.spends += q.build().execute(&mut *tx).await?.rows_affected();
            }

            tx.commit().await?;
            Ok(report)
        })
    }

    // ------------------------------------------------------------- reading

    /// The account's outputs received at or above `min_height`, in chain order.
    pub async fn outputs(&self, account: AccountId, min_height: u64) -> Result<Vec<Output>> {
        dispatch!(self, pool => {
            let rows = sqlx::query(&format!(
                "SELECT {OUTPUT_COLUMNS} FROM outputs WHERE account_id = $1 AND height >= $2 \
                 ORDER BY height, tx_hash, out_index"))
                .bind(account).bind(to_db(min_height)).fetch_all(pool).await?;
            rows.iter().map(output_from_row).collect()
        })
    }

    /// The account's candidate spends at or above `min_height`, in chain order.
    pub async fn spends(&self, account: AccountId, min_height: u64) -> Result<Vec<Spend>> {
        dispatch!(self, pool => {
            let rows = sqlx::query(&format!(
                "SELECT {SPEND_COLUMNS} FROM spends WHERE account_id = $1 AND height >= $2 \
                 ORDER BY height, tx_hash, key_image, source_amount, source_index"))
                .bind(account).bind(to_db(min_height)).fetch_all(pool).await?;
            rows.iter().map(spend_from_row).collect()
        })
    }

    pub async fn counts(&self) -> Result<(u64, u64, u64)> {
        dispatch!(self, pool => {
            let a = sqlx::query("SELECT COUNT(*) AS n FROM accounts").fetch_one(pool).await?.i64_("n")?;
            let o = sqlx::query("SELECT COUNT(*) AS n FROM outputs").fetch_one(pool).await?.i64_("n")?;
            let s = sqlx::query("SELECT COUNT(*) AS n FROM spends").fetch_one(pool).await?.i64_("n")?;
            Ok((a as u64, o as u64, s as u64))
        })
    }

    // -------------------------------------------------------------- leases

    /// Takes or renews the lease `name` for `holder` until `now + ttl`.
    /// Returns false while another holder's lease is unexpired.
    pub async fn acquire_lease(&self, name: &str, holder: &str, ttl: Duration) -> Result<bool> {
        let now = lws_core::now_secs();
        let expires = now + ttl.as_secs() as i64;
        dispatch!(self, pool => {
            let taken = sqlx::query(
                "INSERT INTO leases (name, holder, expires_at) VALUES ($1, $2, $3) \
                 ON CONFLICT (name) DO UPDATE SET holder = excluded.holder, expires_at = excluded.expires_at \
                 WHERE leases.holder = excluded.holder OR leases.expires_at < $4")
                .bind(name).bind(holder).bind(expires).bind(now)
                .execute(pool).await?.rows_affected();
            Ok(taken == 1)
        })
    }

    pub async fn release_lease(&self, name: &str, holder: &str) -> Result<()> {
        dispatch!(self, pool => {
            sqlx::query("DELETE FROM leases WHERE name = $1 AND holder = $2").bind(name).bind(holder).execute(pool).await?;
        });
        Ok(())
    }
}

mod ops;

#[cfg(test)]
mod tests;
