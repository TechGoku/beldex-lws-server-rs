//! Reading the LMDB server's database, for migration without its tools.
//!
//! Only accounts and pending requests are read; everything else is rebuilt
//! from the chain. Records are the fixed-size structs of
//! `src/lws/src/db/data.h` on `lws-implementation`, stored native-endian:
//!
//! ```text
//! account (128 bytes)       request_info (112 bytes)
//!   0  u32 id                 0  [32] view_public
//!   4  u32 access             32 [32] spend_public
//!   8  [32] view_public       64 [32] view key
//!   40 [32] spend_public      96 u64 start_height
//!   72 [32] view key          104 u32 creation
//!   104 u64 scan_height       108 u8 flags, 3 reserved
//!   112 u64 start_height
//!   120 u32 creation
//!   124 u8 flags, 3 reserved
//! ```

use std::path::Path;

use anyhow::{bail, Context, Result};
use lmdb::{Cursor, Environment, EnvironmentFlags, Transaction};
use lws_core::AccountStatus;

const ACCOUNTS: &str = "accounts_by_status,id";
const REQUESTS: &str = "requests_by_type,address";
const ACCOUNT_LEN: usize = 128;
const REQUEST_LEN: usize = 112;

#[derive(Debug)]
pub struct LegacyAccount {
    pub view_public: [u8; 32],
    pub spend_public: [u8; 32],
    pub view_key: zeroize::Zeroizing<[u8; 32]>,
    pub status: AccountStatus,
    pub flags: u8,
    pub scan_height: u64,
    pub start_height: u64,
}

#[derive(Debug)]
pub struct LegacyRequest {
    /// 0 = create, 1 = import (rescan from genesis).
    pub kind: u8,
    pub view_public: [u8; 32],
    pub spend_public: [u8; 32],
    pub view_key: zeroize::Zeroizing<[u8; 32]>,
    pub start_height: u64,
    pub flags: u8,
    pub creation: u32,
}

#[derive(Debug, Default)]
pub struct LegacyDb {
    pub accounts: Vec<LegacyAccount>,
    pub requests: Vec<LegacyRequest>,
}

fn key32(b: &[u8], at: usize) -> [u8; 32] {
    let mut k = [0u8; 32];
    k.copy_from_slice(&b[at..at + 32]);
    k
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

pub fn parse_account(status: u8, b: &[u8]) -> Result<LegacyAccount> {
    if b.len() != ACCOUNT_LEN {
        bail!("account record is {} bytes, expected {ACCOUNT_LEN}", b.len());
    }
    let status = match status {
        0 => AccountStatus::Active,
        1 => AccountStatus::Inactive,
        2 => AccountStatus::Hidden,
        other => bail!("unknown account status {other}"),
    };
    Ok(LegacyAccount {
        view_public: key32(b, 8),
        spend_public: key32(b, 40),
        view_key: zeroize::Zeroizing::new(key32(b, 72)),
        scan_height: u64_at(b, 104),
        start_height: u64_at(b, 112),
        flags: b[124],
        status,
    })
}

pub fn parse_request(kind: u8, b: &[u8]) -> Result<LegacyRequest> {
    if b.len() != REQUEST_LEN {
        bail!("request record is {} bytes, expected {REQUEST_LEN}", b.len());
    }
    if kind > 1 {
        bail!("unknown request type {kind}");
    }
    Ok(LegacyRequest {
        kind,
        view_public: key32(b, 0),
        spend_public: key32(b, 32),
        view_key: zeroize::Zeroizing::new(key32(b, 64)),
        start_height: u64_at(b, 96),
        creation: u32_at(b, 104),
        flags: b[108],
    })
}

fn open(path: &Path, nolock: bool) -> Result<Environment, lmdb::Error> {
    let mut flags = EnvironmentFlags::READ_ONLY;
    if nolock {
        flags |= EnvironmentFlags::NO_LOCK;
    }
    Environment::new().set_flags(flags).set_max_dbs(32).open(path)
}

/// Reads accounts and pending requests from an LMDB server database
/// directory (the one holding `data.mdb`). Opens it read-only; if the lock
/// file was written by an incompatible LMDB build, falls back to lock-free
/// reading, which is only safe while the old server is stopped.
pub fn read(path: &Path) -> Result<LegacyDb> {
    if !path.join("data.mdb").exists() {
        bail!("no LMDB database at {} (expected data.mdb there)", path.display());
    }
    let env = match open(path, false) {
        Ok(env) => env,
        Err(lmdb::Error::VersionMismatch) | Err(lmdb::Error::Invalid) => {
            tracing::warn!("lock file format differs; reading without locks (stop the old server first)");
            open(path, true).with_context(|| format!("opening {}", path.display()))?
        }
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    // Table handles must exist before the transaction that uses them begins.
    let accounts = env.open_db(Some(ACCOUNTS)).with_context(|| format!("table {ACCOUNTS}"))?;
    let requests = env.open_db(Some(REQUESTS)).ok();
    let txn = env.begin_ro_txn().context("starting a read transaction")?;
    let mut out = LegacyDb::default();

    let mut cursor = txn.open_ro_cursor(accounts).context("opening an accounts cursor")?;
    for item in cursor.iter_start() {
        let (key, value) = item?;
        let status = *key.first().context("empty account key")?;
        out.accounts.push(parse_account(status, value)?);
    }
    drop(cursor);

    if let Some(requests) = requests {
        let mut cursor = txn.open_ro_cursor(requests).context("opening a requests cursor")?;
        for item in cursor.iter_start() {
            let (key, value) = item?;
            let kind = *key.first().context("empty request key")?;
            out.requests.push(parse_request(kind, value)?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_records_at_the_documented_offsets() {
        let mut a = vec![0u8; ACCOUNT_LEN];
        a[0..4].copy_from_slice(&7u32.to_le_bytes());
        a[4..8].copy_from_slice(&99u32.to_le_bytes());
        a[8..40].fill(1);
        a[40..72].fill(2);
        a[72..104].fill(3);
        a[104..112].copy_from_slice(&500u64.to_le_bytes());
        a[112..120].copy_from_slice(&400u64.to_le_bytes());
        a[120..124].copy_from_slice(&1234u32.to_le_bytes());
        a[124] = 3;
        let p = parse_account(2, &a).unwrap();
        assert_eq!((p.view_public, p.spend_public, *p.view_key), ([1; 32], [2; 32], [3; 32]));
        assert_eq!((p.scan_height, p.start_height, p.flags), (500, 400, 3));
        assert_eq!(p.status, AccountStatus::Hidden);
        assert!(parse_account(9, &a).is_err());
        assert!(parse_account(0, &a[..100]).is_err());

        let mut r = vec![0u8; REQUEST_LEN];
        r[0..32].fill(4);
        r[32..64].fill(5);
        r[64..96].fill(6);
        r[96..104].copy_from_slice(&42u64.to_le_bytes());
        r[108] = 2;
        let q = parse_request(1, &r).unwrap();
        assert_eq!(
            (q.kind, q.view_public, q.spend_public, *q.view_key, q.start_height, q.flags),
            (1, [4; 32], [5; 32], [6; 32], 42, 2)
        );
    }
}
