//! Safe bindings to the C++ scan kernel (`kernel/include/lws_kernel.h`).
//!
//! The kernel owns every consensus-format and curve operation; this crate only
//! marshals data across the boundary and turns status codes into errors.

use std::ffi::c_char;
use std::fmt;
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

pub mod fast;
pub mod ffi;

pub use ffi::{lws_block as Block, lws_input as Input, lws_match as Match, lws_output as Output, lws_tx as Tx};

pub type Key = [u8; 32];

pub const FLAG_COINBASE: u8 = ffi::LWS_FLAG_COINBASE;
pub const FLAG_RINGCT: u8 = ffi::LWS_FLAG_RINGCT;
pub const FLAG_TOKEN: u8 = ffi::LWS_FLAG_TOKEN;
pub const INPUT_KEY: u8 = ffi::LWS_INPUT_KEY;
pub const INPUT_TOKEN: u8 = ffi::LWS_INPUT_TOKEN;
pub const OUTPUT_KEY: u8 = ffi::LWS_OUTPUT_KEY;
pub const OUTPUT_TOKEN: u8 = ffi::LWS_OUTPUT_TOKEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Network {
    Mainnet,
    Testnet,
    Devnet,
}

impl Network {
    fn code(self) -> u8 {
        match self {
            Network::Mainnet => ffi::LWS_NET_MAINNET,
            Network::Testnet => ffi::LWS_NET_TESTNET,
            Network::Devnet => ffi::LWS_NET_DEVNET,
        }
    }

    /// The `nettype` string beldexd reports in `get_info`.
    pub fn daemon_name(self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
            Network::Devnet => "devnet",
        }
    }
}

impl std::str::FromStr for Network {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "main" | "mainnet" => Ok(Network::Mainnet),
            "test" | "testnet" => Ok(Network::Testnet),
            "dev" | "devnet" => Ok(Network::Devnet),
            other => Err(format!("unknown network '{other}' (expected main, test or dev)")),
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.daemon_name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Argument,
    BlockParse,
    TxParse,
    UnsupportedTx,
    Indices,
    Crypto,
    Address,
    Internal,
    Abi,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            ErrorKind::Argument => "invalid argument",
            ErrorKind::BlockParse => "block parse failed",
            ErrorKind::TxParse => "transaction parse failed",
            ErrorKind::UnsupportedTx => "unsupported transaction",
            ErrorKind::Indices => "output indices mismatch",
            ErrorKind::Crypto => "crypto failure",
            ErrorKind::Address => "invalid address",
            ErrorKind::Internal => "kernel internal error",
            ErrorKind::Abi => "kernel ABI mismatch",
        };
        f.write_str(name)
    }
}

fn last_error(code: i32) -> Error {
    let mut buf = [0 as c_char; 512];
    unsafe { ffi::lws_last_error(buf.as_mut_ptr(), buf.len()) };
    let message = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    let kind = match code {
        ffi::LWS_ERR_ARGUMENT => ErrorKind::Argument,
        ffi::LWS_ERR_BLOCK_PARSE => ErrorKind::BlockParse,
        ffi::LWS_ERR_TX_PARSE => ErrorKind::TxParse,
        ffi::LWS_ERR_UNSUPPORTED_TX => ErrorKind::UnsupportedTx,
        ffi::LWS_ERR_INDICES => ErrorKind::Indices,
        ffi::LWS_ERR_CRYPTO => ErrorKind::Crypto,
        ffi::LWS_ERR_ADDRESS => ErrorKind::Address,
        _ => ErrorKind::Internal,
    };
    Error { kind, message }
}

fn check(code: i32) -> Result<(), Error> {
    if code == ffi::LWS_OK {
        Ok(())
    } else {
        Err(last_error(code))
    }
}

/// Verifies the loaded kernel matches the structs compiled into this crate.
/// Every entry point calls it once; binaries should call it at startup so a
/// mismatched shared library fails before any work is done.
pub fn self_check() -> Result<(), Error> {
    static RESULT: OnceLock<Result<(), Error>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            let abi = unsafe { ffi::lws_kernel_abi_version() };
            if abi != ffi::LWS_KERNEL_ABI_VERSION {
                return Err(Error {
                    kind: ErrorKind::Abi,
                    message: format!("kernel ABI {abi}, expected {}", ffi::LWS_KERNEL_ABI_VERSION),
                });
            }
            let expected = ffi::struct_sizes();
            let mut actual = [0usize; 10];
            let count = unsafe { ffi::lws_struct_sizes(actual.as_mut_ptr(), actual.len()) };
            if count != expected.len() || actual != expected {
                return Err(Error {
                    kind: ErrorKind::Abi,
                    message: format!("struct sizes {actual:?}, expected {expected:?}"),
                });
            }
            Ok(())
        })
        .clone()
}

/// One `get_blocks.bin` entry, borrowed from the decoded response.
pub struct BlockEntry<'a> {
    pub block: &'a [u8],
    pub txs: Vec<&'a [u8]>,
    /// Global output indices per tx, miner tx first.
    pub output_indices: Vec<Vec<u64>>,
}

/// A parsed run of blocks. Immutable, so it may be scanned from many threads.
pub struct Batch {
    ptr: NonNull<ffi::lws_batch>,
}

// The kernel never mutates a batch after lws_parse_blocks returns.
unsafe impl Send for Batch {}
unsafe impl Sync for Batch {}

impl Drop for Batch {
    fn drop(&mut self) {
        unsafe { ffi::lws_batch_free(self.ptr.as_ptr()) }
    }
}

pub struct BatchView<'a> {
    pub blocks: &'a [Block],
    pub txs: &'a [Tx],
    pub inputs: &'a [Input],
    pub outputs: &'a [Output],
    pub ring_members: &'a [u64],
}

/// An account as the scanner sees it.
#[derive(Clone)]
pub struct ScanKey {
    pub view_secret: Key,
    pub spend_public: Key,
    pub scan_height: u64,
}

impl Drop for ScanKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.view_secret);
    }
}

unsafe fn slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if len == 0 || ptr.is_null() {
        &[]
    } else {
        std::slice::from_raw_parts(ptr, len)
    }
}

impl Batch {
    pub fn parse(entries: &[BlockEntry<'_>], pruned: bool) -> Result<Batch, Error> {
        self_check()?;
        // Flatten into the C layout. These vectors must outlive the call only.
        let tx_blobs: Vec<Vec<ffi::lws_blob>> = entries
            .iter()
            .map(|e| e.txs.iter().map(|t| ffi::lws_blob { data: t.as_ptr(), len: t.len() }).collect())
            .collect();
        let flat_indices: Vec<Vec<u64>> =
            entries.iter().map(|e| e.output_indices.iter().flatten().copied().collect()).collect();
        let counts: Vec<Vec<u32>> = entries
            .iter()
            .map(|e| e.output_indices.iter().map(|tx| u32::try_from(tx.len()).unwrap_or(u32::MAX)).collect())
            .collect();
        let c_entries: Vec<ffi::lws_block_entry> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| ffi::lws_block_entry {
                block: ffi::lws_blob { data: e.block.as_ptr(), len: e.block.len() },
                txs: tx_blobs[i].as_ptr(),
                tx_count: tx_blobs[i].len(),
                indices: flat_indices[i].as_ptr(),
                index_counts: counts[i].as_ptr(),
                index_count_len: counts[i].len(),
            })
            .collect();

        let mut out: *mut ffi::lws_batch = ptr::null_mut();
        check(unsafe { ffi::lws_parse_blocks(c_entries.as_ptr(), c_entries.len(), pruned as i32, &mut out) })?;
        NonNull::new(out)
            .map(|ptr| Batch { ptr })
            .ok_or_else(|| Error { kind: ErrorKind::Internal, message: "null batch".into() })
    }

    pub fn view(&self) -> BatchView<'_> {
        let mut v = ffi::lws_batch_view::default();
        unsafe {
            ffi::lws_batch_get(self.ptr.as_ptr(), &mut v);
            BatchView {
                blocks: slice(v.blocks, v.block_count),
                txs: slice(v.txs, v.tx_count),
                inputs: slice(v.inputs, v.input_count),
                outputs: slice(v.outputs, v.output_count),
                ring_members: slice(v.ring_members, v.ring_member_count),
            }
        }
    }

    /// Finds every output in the batch owned by one of `keys`. `Match::account`
    /// indexes into `keys`.
    pub fn scan(&self, keys: &[ScanKey]) -> Result<Vec<Match>, Error> {
        let accounts: Vec<ffi::lws_account> = keys
            .iter()
            .map(|k| ffi::lws_account {
                view_secret: ffi::lws_key { data: k.view_secret },
                spend_public: ffi::lws_key { data: k.spend_public },
                scan_height: k.scan_height,
            })
            .collect();
        let mut out: *mut ffi::lws_match = ptr::null_mut();
        let mut count = 0usize;
        let rc = unsafe { ffi::lws_scan(self.ptr.as_ptr(), accounts.as_ptr(), accounts.len(), &mut out, &mut count) };
        // Wipe the copies of the view keys before anything else can fail.
        let mut accounts = accounts;
        for a in &mut accounts {
            zeroize::Zeroize::zeroize(&mut a.view_secret.data);
        }
        check(rc)?;
        let matches = unsafe { slice(out, count) }.to_vec();
        unsafe { ffi::lws_matches_free(out) };
        Ok(matches)
    }

    /// Checks only the given `(tx index, key index)` pairs, e.g. a prefilter's
    /// candidates. `Match::account` indexes into `keys`.
    pub fn scan_pairs(&self, keys: &[ScanKey], pairs: &[(u32, u32)]) -> Result<Vec<Match>, Error> {
        let mut accounts: Vec<ffi::lws_account> = keys
            .iter()
            .map(|k| ffi::lws_account {
                view_secret: ffi::lws_key { data: k.view_secret },
                spend_public: ffi::lws_key { data: k.spend_public },
                scan_height: k.scan_height,
            })
            .collect();
        let txs: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        let owners: Vec<u32> = pairs.iter().map(|p| p.1).collect();
        let mut out: *mut ffi::lws_match = ptr::null_mut();
        let mut count = 0usize;
        let rc = unsafe {
            ffi::lws_scan_pairs(
                self.ptr.as_ptr(),
                accounts.as_ptr(),
                accounts.len(),
                txs.as_ptr(),
                owners.as_ptr(),
                pairs.len(),
                &mut out,
                &mut count,
            )
        };
        for a in &mut accounts {
            zeroize::Zeroize::zeroize(&mut a.view_secret.data);
        }
        check(rc)?;
        let matches = unsafe { slice(out, count) }.to_vec();
        unsafe { ffi::lws_matches_free(out) };
        Ok(matches)
    }
}

impl Tx {
    pub fn inputs<'a>(&self, view: &BatchView<'a>) -> &'a [Input] {
        let start = self.input_offset as usize;
        &view.inputs[start..start + self.input_count as usize]
    }
    pub fn outputs<'a>(&self, view: &BatchView<'a>) -> &'a [Output] {
        let start = self.output_offset as usize;
        &view.outputs[start..start + self.output_count as usize]
    }
}

impl Input {
    pub fn ring<'a>(&self, view: &BatchView<'a>) -> &'a [u64] {
        let start = self.ring_offset as usize;
        &view.ring_members[start..start + self.ring_count as usize]
    }
}

impl Block {
    pub fn txs<'a>(&self, view: &BatchView<'a>) -> &'a [Tx] {
        let start = self.tx_offset as usize;
        &view.txs[start..start + self.tx_count as usize]
    }
}

impl Match {
    pub fn rct(&self) -> Option<&[u8; 96]> {
        (self.has_rct != 0).then_some(&self.rct)
    }

    pub fn payment_id(&self) -> Option<&[u8]> {
        match self.payment_id_len {
            0 => None,
            n => Some(&self.payment_id[..n as usize]),
        }
    }
}

/// Decodes a standard address for `network`. Subaddresses and integrated
/// addresses are rejected: the server derives outputs for the primary address only.
pub fn decode_address(address: &str, network: Network) -> Result<(Key, Key), Error> {
    let mut view = ffi::lws_key::default();
    let mut spend = ffi::lws_key::default();
    check(unsafe {
        ffi::lws_address_decode(address.as_ptr().cast(), address.len(), network.code(), &mut view, &mut spend)
    })?;
    Ok((view.data, spend.data))
}

pub fn encode_address(view_public: &Key, spend_public: &Key, network: Network) -> String {
    let view = ffi::lws_key { data: *view_public };
    let spend = ffi::lws_key { data: *spend_public };
    let mut buf = vec![0u8; 256];
    let len = unsafe { ffi::lws_address_encode(&view, &spend, network.code(), buf.as_mut_ptr().cast(), buf.len()) };
    buf.truncate(len.min(buf.len() - 1));
    String::from_utf8(buf).unwrap_or_default()
}

/// True when `view_secret` is a canonical scalar whose public key is `view_public`.
pub fn view_key_matches(view_secret: &Key, view_public: &Key) -> bool {
    let secret = ffi::lws_key { data: *view_secret };
    let public = ffi::lws_key { data: *view_public };
    unsafe { ffi::lws_view_key_matches(&secret, &public) == 1 }
}

pub fn secret_to_public(secret: &Key) -> Result<Key, Error> {
    let secret = ffi::lws_key { data: *secret };
    let mut public = ffi::lws_key::default();
    check(unsafe { ffi::lws_secret_to_public(&secret, &mut public) })?;
    Ok(public.data)
}

pub fn generate_keypair() -> (Key, Key) {
    let mut secret = ffi::lws_key::default();
    let mut public = ffi::lws_key::default();
    unsafe { ffi::lws_generate_keypair(&mut secret, &mut public) };
    (secret.data, public.data)
}

/// The 96-byte `rct` value the lightwallet API returns for an owned output.
pub fn output_rct(
    tx_pub_key: &Key,
    view_secret: &Key,
    output_index: u32,
    amount: u64,
    mask: &Key,
) -> Result<[u8; 96], Error> {
    let tx_pub = ffi::lws_key { data: *tx_pub_key };
    let secret = ffi::lws_key { data: *view_secret };
    let mask = ffi::lws_key { data: *mask };
    let mut out = [0u8; 96];
    check(unsafe { ffi::lws_output_rct(&tx_pub, &secret, output_index, amount, &mask, out.as_mut_ptr()) })?;
    Ok(out)
}

/// `(tx_pub_key, output_key)` a sender would produce, via beldex core.
pub fn derive_output_key(
    tx_secret: &Key,
    view_public: &Key,
    output_index: u32,
    spend_public: &Key,
) -> Result<(Key, Key), Error> {
    let (r, v, b) =
        (ffi::lws_key { data: *tx_secret }, ffi::lws_key { data: *view_public }, ffi::lws_key { data: *spend_public });
    let (mut tx_pub, mut out) = (ffi::lws_key::default(), ffi::lws_key::default());
    check(unsafe { ffi::lws_derive_output_key(&r, &v, output_index, &b, &mut tx_pub, &mut out) })?;
    Ok((tx_pub.data, out.data))
}

pub fn commit(amount: u64, mask: &Key) -> Result<Key, Error> {
    let mask = ffi::lws_key { data: *mask };
    let mut out = ffi::lws_key::default();
    check(unsafe { ffi::lws_commit(amount, &mask, &mut out) })?;
    Ok(out.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_kernel() {
        self_check().unwrap();
    }

    #[test]
    fn keys_and_addresses() {
        let (view_sec, view_pub) = generate_keypair();
        let (_, spend_pub) = generate_keypair();
        assert_eq!(secret_to_public(&view_sec).unwrap(), view_pub);
        assert!(view_key_matches(&view_sec, &view_pub));
        assert!(!view_key_matches(&view_sec, &spend_pub));

        for net in [Network::Mainnet, Network::Testnet, Network::Devnet] {
            let text = encode_address(&view_pub, &spend_pub, net);
            assert!(!text.is_empty());
            assert_eq!(decode_address(&text, net).unwrap(), (view_pub, spend_pub));
        }
        let main = encode_address(&view_pub, &spend_pub, Network::Mainnet);
        let err = decode_address(&main, Network::Testnet).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Address);
    }

    #[test]
    fn rejects_garbage_block() {
        let junk = [1u8, 2, 3];
        let entry = BlockEntry { block: &junk, txs: vec![], output_indices: vec![vec![]] };
        let err = Batch::parse(&[entry], true).err().unwrap();
        assert_eq!(err.kind, ErrorKind::BlockParse);
    }

    #[test]
    fn rct_blob_commits_to_amount() {
        let (view_sec, _) = generate_keypair();
        let (_, tx_pub) = generate_keypair();
        let (mask, _) = generate_keypair();
        let blob = output_rct(&tx_pub, &view_sec, 3, 42, &mask).unwrap();
        assert_eq!(&blob[..32], &commit(42, &mask).unwrap());
    }
}
