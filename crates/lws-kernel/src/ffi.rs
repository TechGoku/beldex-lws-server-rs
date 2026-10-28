//! Raw declarations mirroring `kernel/include/lws_kernel.h`. Field order and
//! types must match the header exactly; `self_check` verifies the sizes.
#![allow(non_camel_case_types)]

use std::ffi::c_char;

pub const LWS_KERNEL_ABI_VERSION: u32 = 4;

pub const LWS_OK: i32 = 0;
pub const LWS_ERR_ARGUMENT: i32 = 1;
pub const LWS_ERR_BLOCK_PARSE: i32 = 2;
pub const LWS_ERR_TX_PARSE: i32 = 3;
pub const LWS_ERR_UNSUPPORTED_TX: i32 = 4;
pub const LWS_ERR_INDICES: i32 = 5;
pub const LWS_ERR_CRYPTO: i32 = 6;
pub const LWS_ERR_ADDRESS: i32 = 7;

pub const LWS_NET_MAINNET: u8 = 0;
pub const LWS_NET_TESTNET: u8 = 1;
pub const LWS_NET_DEVNET: u8 = 2;

pub const LWS_INPUT_KEY: u8 = 1;
pub const LWS_INPUT_TOKEN: u8 = 2;
pub const LWS_OUTPUT_KEY: u8 = 1;
pub const LWS_OUTPUT_TOKEN: u8 = 2;

pub const LWS_FLAG_COINBASE: u8 = 0x01;
pub const LWS_FLAG_RINGCT: u8 = 0x02;
pub const LWS_FLAG_TOKEN: u8 = 0x04;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct lws_key {
    pub data: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct lws_blob {
    pub data: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct lws_block_entry {
    pub block: lws_blob,
    pub txs: *const lws_blob,
    pub tx_count: usize,
    pub indices: *const u64,
    pub index_counts: *const u32,
    pub index_count_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lws_block {
    pub height: u64,
    pub timestamp: u64,
    pub hash: lws_key,
    pub prev_hash: lws_key,
    pub tx_offset: u32,
    pub tx_count: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lws_tx {
    pub hash: lws_key,
    pub prefix_hash: lws_key,
    pub tx_pub_key: lws_key,
    pub locked_key_image: lws_key,
    pub unlock_time: u64,
    pub input_offset: u32,
    pub input_count: u32,
    pub output_offset: u32,
    pub output_count: u32,
    pub mixin: u32,
    pub version: u16,
    pub type_: u16,
    pub has_tx_pub_key: u8,
    pub has_locked_key_image: u8,
    pub is_coinbase: u8,
    pub long_payment_id_len: u8,
    pub long_payment_id: [u8; 32],
    pub has_short_payment_id: u8,
    pub short_payment_id: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lws_input {
    pub key_image: lws_key,
    pub amount: u64,
    pub ring_offset: u32,
    pub ring_count: u32,
    pub kind: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lws_output {
    pub pub_key: lws_key,
    pub commitment: lws_key,
    pub blinded_token_id: lws_key,
    pub amount: u64,
    pub global_index: u64,
    pub unlock_time: u64,
    pub encrypted_amount: u64,
    pub index: u32,
    pub native_index: i32,
    pub kind: u8,
}

pub enum lws_batch {}

#[repr(C)]
pub struct lws_batch_view {
    pub blocks: *const lws_block,
    pub block_count: usize,
    pub txs: *const lws_tx,
    pub tx_count: usize,
    pub inputs: *const lws_input,
    pub input_count: usize,
    pub outputs: *const lws_output,
    pub output_count: usize,
    pub ring_members: *const u64,
    pub ring_member_count: usize,
}

impl Default for lws_batch_view {
    fn default() -> Self {
        Self {
            blocks: std::ptr::null(),
            block_count: 0,
            txs: std::ptr::null(),
            tx_count: 0,
            inputs: std::ptr::null(),
            input_count: 0,
            outputs: std::ptr::null(),
            output_count: 0,
            ring_members: std::ptr::null(),
            ring_member_count: 0,
        }
    }
}

#[repr(C)]
pub struct lws_account {
    pub view_secret: lws_key,
    pub spend_public: lws_key,
    pub scan_height: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct lws_match {
    pub account: u32,
    pub tx: u32,
    pub output: u32,
    pub flags: u8,
    pub payment_id_len: u8,
    pub payment_id: [u8; 32],
    pub has_rct: u8,
    pub amount: u64,
    pub mask: lws_key,
    pub token_id: lws_key,
    pub rct: [u8; 96],
}

pub fn struct_sizes() -> [usize; 10] {
    use std::mem::size_of;
    [
        size_of::<lws_key>(),
        size_of::<lws_blob>(),
        size_of::<lws_block_entry>(),
        size_of::<lws_block>(),
        size_of::<lws_tx>(),
        size_of::<lws_input>(),
        size_of::<lws_output>(),
        size_of::<lws_batch_view>(),
        size_of::<lws_account>(),
        size_of::<lws_match>(),
    ]
}

extern "C" {
    pub fn lws_kernel_abi_version() -> u32;
    pub fn lws_struct_sizes(out: *mut usize, n: usize) -> usize;
    pub fn lws_last_error(out: *mut c_char, out_len: usize) -> usize;
    pub fn lws_parse_blocks(
        entries: *const lws_block_entry,
        entry_count: usize,
        pruned: i32,
        out: *mut *mut lws_batch,
    ) -> i32;
    pub fn lws_batch_get(batch: *const lws_batch, out: *mut lws_batch_view);
    pub fn lws_batch_free(batch: *mut lws_batch);
    pub fn lws_scan(
        batch: *const lws_batch,
        accounts: *const lws_account,
        account_count: usize,
        out: *mut *mut lws_match,
        out_count: *mut usize,
    ) -> i32;
    pub fn lws_matches_free(matches: *mut lws_match);
    pub fn lws_scan_pairs(
        batch: *const lws_batch,
        accounts: *const lws_account,
        account_count: usize,
        pair_tx: *const u32,
        pair_account: *const u32,
        pair_count: usize,
        out: *mut *mut lws_match,
        out_count: *mut usize,
    ) -> i32;
    pub fn lws_address_decode(
        address: *const c_char,
        len: usize,
        network: u8,
        view_public: *mut lws_key,
        spend_public: *mut lws_key,
    ) -> i32;
    pub fn lws_address_encode(
        view_public: *const lws_key,
        spend_public: *const lws_key,
        network: u8,
        out: *mut c_char,
        out_len: usize,
    ) -> usize;
    pub fn lws_view_key_matches(view_secret: *const lws_key, view_public: *const lws_key) -> i32;
    pub fn lws_secret_to_public(secret: *const lws_key, public_out: *mut lws_key) -> i32;
    pub fn lws_generate_keypair(secret_out: *mut lws_key, public_out: *mut lws_key);
    pub fn lws_output_rct(
        tx_pub_key: *const lws_key,
        view_secret: *const lws_key,
        output_index: u32,
        amount: u64,
        mask: *const lws_key,
        out: *mut u8,
    ) -> i32;
    pub fn lws_derive_output_key(
        tx_secret: *const lws_key,
        view_public: *const lws_key,
        output_index: u32,
        spend_public: *const lws_key,
        tx_pub_out: *mut lws_key,
        output_key_out: *mut lws_key,
    ) -> i32;
    pub fn lws_commit(amount: u64, mask: *const lws_key, out: *mut lws_key) -> i32;
}
