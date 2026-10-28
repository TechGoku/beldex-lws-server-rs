//! Domain types shared by the store, scanner and API.
//!
//! Amounts and unlock times are `u64` on chain. Databases only offer signed
//! 64-bit integers, so they are stored bit-cast (see [`to_db`] / [`from_db`])
//! and are never summed in SQL; every total is computed here with overflow
//! checks.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub mod metrics;

pub use lws_kernel::{Key, Network};

/// Unlock times at or above this are UNIX timestamps, below are heights.
pub const MAX_BLOCK_NUMBER: u64 = 500_000_000;

pub type AccountId = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStatus {
    /// Scanned and served.
    Active,
    /// Not scanned, still served.
    Inactive,
    /// Neither scanned nor served.
    Hidden,
}

impl AccountStatus {
    pub fn to_db(self) -> i16 {
        match self {
            AccountStatus::Active => 0,
            AccountStatus::Inactive => 1,
            AccountStatus::Hidden => 2,
        }
    }
    pub fn from_db(value: i16) -> Option<Self> {
        match value {
            0 => Some(AccountStatus::Active),
            1 => Some(AccountStatus::Inactive),
            2 => Some(AccountStatus::Hidden),
            _ => None,
        }
    }
}

impl std::str::FromStr for AccountStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(AccountStatus::Active),
            "inactive" => Ok(AccountStatus::Inactive),
            "hidden" => Ok(AccountStatus::Hidden),
            other => Err(format!("unknown account status '{other}'")),
        }
    }
}

pub mod account_flags {
    pub const ADMIN: i16 = 1;
    pub const GENERATED_LOCALLY: i16 = 2;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Create a new account (only when account creation needs approval).
    Create,
    /// Rescan an existing account from genesis.
    ImportScan,
}

impl RequestKind {
    pub fn to_db(self) -> i16 {
        match self {
            RequestKind::Create => 0,
            RequestKind::ImportScan => 1,
        }
    }
    pub fn from_db(value: i16) -> Option<Self> {
        match value {
            0 => Some(RequestKind::Create),
            1 => Some(RequestKind::ImportScan),
            _ => None,
        }
    }
}

/// A secret view key. Wiped from memory on drop and never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ViewKey(Zeroizing<Key>);

impl ViewKey {
    pub fn new(bytes: Key) -> Self {
        ViewKey(Zeroizing::new(bytes))
    }
    pub fn bytes(&self) -> &Key {
        &self.0
    }
    pub fn from_hex(text: &str) -> Option<Self> {
        let mut out = Zeroizing::new([0u8; 32]);
        hex::decode_to_slice(text, out.as_mut()).ok()?;
        Some(ViewKey(out))
    }
    pub fn to_hex(&self) -> String {
        hex::encode(self.bytes())
    }
}

impl fmt::Debug for ViewKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ViewKey(<redacted>)")
    }
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: AccountId,
    pub address: String,
    pub view_public: Key,
    pub spend_public: Key,
    pub view_key: ViewKey,
    pub status: AccountStatus,
    pub flags: i16,
    pub start_height: u64,
    pub scan_height: u64,
    /// Bumped by every rescan; a scanner commit made against an older
    /// generation is discarded instead of resurrecting rolled-back rows.
    pub generation: i64,
    pub created_at: i64,
    pub last_access: i64,
}

impl Account {
    pub fn is_admin(&self) -> bool {
        self.flags & account_flags::ADMIN != 0
    }
    pub fn generated_locally(&self) -> bool {
        self.flags & account_flags::GENERATED_LOCALLY != 0
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub kind: RequestKind,
    pub address: String,
    pub view_public: Key,
    pub spend_public: Key,
    pub view_key: ViewKey,
    pub start_height: u64,
    pub flags: i16,
    pub created_at: i64,
}

/// Identifies an output on chain the way ring members do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutputRef {
    /// Plaintext amount bucket for pre-ringct outputs, 0 otherwise.
    pub amount: u64,
    pub global_index: u64,
}

pub mod output_flags {
    pub const COINBASE: i16 = 0x01;
    pub const RINGCT: i16 = 0x02;
    pub const TOKEN: i16 = 0x04;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenData {
    pub token_id: Key,
    pub blinded_token_id: Key,
    pub amount_commitment: Key,
    pub encrypted_amount: u64,
}

/// An output received by an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub account_id: AccountId,
    pub height: u64,
    pub tx_hash: Key,
    pub tx_prefix_hash: Key,
    pub tx_pub_key: Key,
    pub out_index: u32,
    pub source: OutputRef,
    pub amount: u64,
    pub mask: Key,
    pub pub_key: Key,
    pub unlock_time: u64,
    pub timestamp: u64,
    pub mixin: u32,
    pub flags: i16,
    pub payment_id: Option<Vec<u8>>,
    pub locked_key_image: Option<Key>,
    pub token: Option<TokenData>,
    /// The 96-byte `rct` blob served to wallets, computed once at scan time.
    pub rct: Option<Vec<u8>>,
}

impl Output {
    pub fn is_coinbase(&self) -> bool {
        self.flags & output_flags::COINBASE != 0
    }
    pub fn token_id(&self) -> Option<&Key> {
        self.token.as_ref().map(|t| &t.token_id)
    }
}

/// A transaction input that has one of the account's outputs in its ring.
/// The server only has view keys, so this is a *candidate* spend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spend {
    pub account_id: AccountId,
    pub height: u64,
    pub tx_hash: Key,
    pub key_image: Key,
    pub source: OutputRef,
    pub timestamp: u64,
    pub unlock_time: u64,
    pub mixin: u32,
    pub payment_id: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockId {
    pub height: u64,
    pub hash: Key,
}

pub fn to_db(value: u64) -> i64 {
    value as i64
}

pub fn from_db(value: i64) -> u64 {
    value as u64
}

pub fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Whether an output with `unlock_time` is still locked for an account scanned
/// to `scan_height`. Heights compare against the scan height; timestamps
/// against the wall clock.
pub fn is_locked(unlock_time: u64, scan_height: u64, now: i64) -> bool {
    if unlock_time > MAX_BLOCK_NUMBER {
        unlock_time > now.max(0) as u64
    } else {
        unlock_time > scan_height
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the timestamp format the lightwallet API uses.
pub fn iso_timestamp(secs: u64) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    i64::try_from(secs)
        .ok()
        .and_then(|s| time::OffsetDateTime::from_unix_timestamp(s).ok())
        .and_then(|t| t.format(&format).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}

pub fn parse_key(text: &str) -> Option<Key> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(text, &mut out).ok()?;
    Some(out)
}

/// Checked running total for amounts reported to wallets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Total(u128);

impl Total {
    pub fn add(&mut self, amount: u64) {
        self.0 += u128::from(amount);
    }
    /// Saturates instead of wrapping: a total above u64::MAX means corrupt
    /// data, and a pinned maximum is easier to spot than a small wrapped number.
    pub fn get(self) -> u64 {
        u64::try_from(self.0).unwrap_or(u64::MAX)
    }
    /// The exact total, for arithmetic whose result may fit in 64 bits even
    /// when its inputs do not.
    pub fn exact(self) -> u128 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks() {
        assert!(is_locked(100, 99, 0));
        assert!(!is_locked(100, 100, 0));
        assert!(!is_locked(0, 0, 0));
        let ts = MAX_BLOCK_NUMBER + 10_000;
        assert!(is_locked(ts, 1_000_000, (ts - 1) as i64));
        assert!(!is_locked(ts, 0, ts as i64));
    }

    #[test]
    fn timestamps() {
        assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_timestamp(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn db_casts_round_trip() {
        for v in [0u64, 1, i64::MAX as u64, u64::MAX] {
            assert_eq!(from_db(to_db(v)), v);
        }
    }

    #[test]
    fn totals_do_not_wrap() {
        let mut t = Total::default();
        t.add(u64::MAX);
        t.add(5);
        assert_eq!(t.get(), u64::MAX);
    }

    #[test]
    fn view_key_debug_is_redacted() {
        let key = ViewKey::new([7; 32]);
        assert_eq!(format!("{key:?}"), "ViewKey(<redacted>)");
        assert_eq!(ViewKey::from_hex(&key.to_hex()).unwrap(), key);
    }
}
