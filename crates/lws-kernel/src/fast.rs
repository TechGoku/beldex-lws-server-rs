//! Fast ownership prefilter.
//!
//! Deciding whether an account owns an output is the scanner's dominant cost:
//! one variable-base scalar multiplication per (account, transaction), plus
//! one fixed-base multiplication per output. Every account is checked against
//! every transaction, so this is what limits accounts per machine.
//!
//! This module computes the same test as beldex core (`generate_key_derivation`
//! followed by `derive_subaddress_public_key`) with curve25519-dalek, and
//! amortises the expensive part: for each transaction a precomputed table of
//! its public key is built once and shared by every account, which turns the
//! variable-base multiplication into a fixed-base one.
//!
//! It is only a filter. Accounts it flags are scanned again by the C++ kernel,
//! which alone decides ownership and decodes amounts and tokens; outputs are
//! never stored on this module's word. The tests check it against the kernel
//! on random keys so it cannot silently miss outputs.

use curve25519_dalek::constants::ED25519_BASEPOINT_TABLE;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsBasepointTable, EdwardsPoint};
use curve25519_dalek::scalar::Scalar;
use curve25519_dalek::traits::BasepointTable;
use rayon::prelude::*;
use sha3::{Digest, Keccak256};

use crate::{BatchView, Key, ScanKey};

/// Below this many accounts a per-transaction table costs more than it saves.
const TABLE_THRESHOLD: usize = 24;
/// Transactions whose tables are held at once (about 30 KB each).
const TX_CHUNK: usize = 256;
/// Accounts per unit of parallel work.
const ACCOUNT_BLOCK: usize = 64;

fn varint(mut v: u64, out: &mut [u8; 10]) -> usize {
    let mut n = 0;
    while v >= 0x80 {
        out[n] = (v as u8 & 0x7f) | 0x80;
        v >>= 7;
        n += 1;
    }
    out[n] = v as u8;
    n + 1
}

/// `H_s(derivation || varint(index))`, as `crypto::derivation_to_scalar`.
fn derivation_to_scalar(derivation: &[u8; 32], index: u32) -> Scalar {
    let mut buf = [0u8; 42];
    buf[..32].copy_from_slice(derivation);
    let mut v = [0u8; 10];
    let n = varint(u64::from(index), &mut v);
    buf[32..32 + n].copy_from_slice(&v[..n]);
    let hash: [u8; 32] = Keccak256::digest(&buf[..32 + n]).into();
    Scalar::from_bytes_mod_order(hash)
}

enum TxKey {
    Table(Box<EdwardsBasepointTable>),
    Point(EdwardsPoint),
}

struct PreparedTx {
    height: u64,
    key: TxKey,
    outputs: Vec<(u32, EdwardsPoint)>,
}

struct PreparedAccount {
    view: Scalar,
    spend: EdwardsPoint,
}

fn decompress(bytes: &Key) -> Option<EdwardsPoint> {
    CompressedEdwardsY(*bytes).decompress()
}

/// Whether `bytes` encodes a curve point (as `crypto::check_key` checks).
pub fn is_point(bytes: &Key) -> bool {
    decompress(bytes).is_some()
}

fn prepare_account(key: &ScanKey) -> Option<PreparedAccount> {
    let view = Option::<Scalar>::from(Scalar::from_canonical_bytes(key.view_secret))?;
    Some(PreparedAccount { view, spend: decompress(&key.spend_public)? })
}

/// Does `account` own any of `outputs` of a tx whose public key is `tx_key`?
fn owns(tx_key: &TxKey, outputs: &[(u32, EdwardsPoint)], account: &PreparedAccount) -> bool {
    let shared = match tx_key {
        TxKey::Table(t) => t.as_ref() * &account.view,
        TxKey::Point(p) => p * account.view,
    };
    let derivation = shared.mul_by_cofactor().compress().to_bytes();
    outputs.iter().any(|(index, point)| {
        let s = derivation_to_scalar(&derivation, *index);
        point - ED25519_BASEPOINT_TABLE * &s == account.spend
    })
}

/// What the prefilter found in a batch.
#[derive(Debug, Default)]
pub struct Candidates {
    /// `(tx index, key index)`: the account may own an output of that tx.
    pub pairs: Vec<(u32, u32)>,
    /// Keys the prefilter could not prepare (not a canonical scalar or point);
    /// these need a full kernel scan. Never seen for keys that passed login.
    pub full_scan: Vec<usize>,
}

/// Screens every (transaction, account) pair in the batch. Runs on the
/// current rayon pool. A pair is reported if the account may own any output
/// of the transaction; the kernel then decides.
pub fn candidates(view: &BatchView<'_>, keys: &[ScanKey]) -> Candidates {
    let accounts: Vec<Option<PreparedAccount>> = keys.par_iter().map(prepare_account).collect();
    let full_scan: Vec<usize> = accounts.iter().enumerate().filter(|(_, a)| a.is_none()).map(|(i, _)| i).collect();
    let use_tables = keys.len() >= TABLE_THRESHOLD;

    let mut txs = Vec::new();
    for block in view.blocks {
        for (t, tx) in block.txs(view).iter().enumerate() {
            if tx.has_tx_pub_key != 0 && tx.output_count > 0 {
                txs.push((block.tx_offset + t as u32, block.height, tx));
            }
        }
    }

    let mut pairs = Vec::new();
    for chunk in txs.chunks(TX_CHUNK) {
        let prepared: Vec<(u32, PreparedTx)> = chunk
            .par_iter()
            .filter_map(|(index, height, tx)| {
                let point = decompress(&tx.tx_pub_key.data)?;
                let outputs: Vec<(u32, EdwardsPoint)> =
                    tx.outputs(view).iter().filter_map(|o| Some((o.index, decompress(&o.pub_key.data)?))).collect();
                if outputs.is_empty() {
                    return None;
                }
                let key = if use_tables {
                    TxKey::Table(Box::new(EdwardsBasepointTable::create(&point)))
                } else {
                    TxKey::Point(point)
                };
                Some((*index, PreparedTx { height: *height, key, outputs }))
            })
            .collect();

        // Work items are (transaction, block of accounts), so every core is
        // busy whether the batch holds many transactions or many accounts.
        let blocks_of_accounts = keys.len().div_ceil(ACCOUNT_BLOCK).max(1);
        let accounts = &accounts;
        let prepared = &prepared;
        pairs.par_extend((0..prepared.len() * blocks_of_accounts).into_par_iter().flat_map_iter(move |item| {
            let (tx_index, tx) = &prepared[item / blocks_of_accounts];
            let first = (item % blocks_of_accounts) * ACCOUNT_BLOCK;
            (first..(first + ACCOUNT_BLOCK).min(keys.len()))
                .filter(move |&i| {
                    tx.height > keys[i].scan_height
                        && accounts[i].as_ref().is_some_and(|a| owns(&tx.key, &tx.outputs, a))
                })
                .map(move |i| (*tx_index, i as u32))
        }));
    }
    pairs.sort_unstable();
    Candidates { pairs, full_scan }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_account() -> (Key, Key, Key) {
        let (view_secret, view_public) = crate::generate_keypair();
        let (_, spend_public) = crate::generate_keypair();
        (view_secret, view_public, spend_public)
    }

    /// The prefilter agrees with beldex core on randomly generated outputs,
    /// including output indices whose varint takes more than one byte.
    #[test]
    fn matches_the_kernel() {
        for case in 0..400u32 {
            let (view_secret, view_public, spend_public) = random_account();
            let (tx_secret, _) = crate::generate_keypair();
            let index = if case % 4 == 0 { case * 37 } else { case % 16 };
            let (tx_pub, out_key) = crate::derive_output_key(&tx_secret, &view_public, index, &spend_public).unwrap();

            let account = prepare_account(&ScanKey { view_secret, spend_public, scan_height: 0 }).unwrap();
            let point = decompress(&tx_pub).unwrap();
            let outputs = vec![(index, decompress(&out_key).unwrap())];
            for key in [TxKey::Point(point), TxKey::Table(Box::new(EdwardsBasepointTable::create(&point)))] {
                assert!(owns(&key, &outputs, &account), "case {case}: missed an owned output");
                let (other_secret, _, other_spend) = random_account();
                let other =
                    prepare_account(&ScanKey { view_secret: other_secret, spend_public: other_spend, scan_height: 0 })
                        .unwrap();
                assert!(!owns(&key, &outputs, &other), "case {case}: claimed someone else's output");
                // Right key, wrong index: not ours.
                assert!(!owns(&key, &[(index + 1, outputs[0].1)], &account));
            }
        }
    }

    #[test]
    fn varints_match_monero() {
        let enc = |v: u64| {
            let mut b = [0u8; 10];
            let n = varint(v, &mut b);
            b[..n].to_vec()
        };
        assert_eq!(enc(0), [0]);
        assert_eq!(enc(127), [127]);
        assert_eq!(enc(128), [0x80, 1]);
        assert_eq!(enc(300), [0xac, 2]);
    }

    #[test]
    fn unpreparable_keys_are_left_to_the_kernel() {
        let view = BatchView { blocks: &[], txs: &[], inputs: &[], outputs: &[], ring_members: &[] };
        let bad = ScanKey { view_secret: [0xff; 32], spend_public: [0; 32], scan_height: 0 };
        assert_eq!(candidates(&view, &[bad]).full_scan, vec![0]);
        let bad = ScanKey { view_secret: [0xff; 32], spend_public: [0; 32], scan_height: 0 };
        assert!(prepare_account(&bad).is_none());
    }
}
