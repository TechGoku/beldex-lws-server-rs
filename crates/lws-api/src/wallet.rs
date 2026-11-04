//! Wallet-facing endpoints. Response shapes follow the reference server
//! (`src/lws/src/rpc/light_wallet.cpp` on `lws-implementation`) field for
//! field, so existing wallets work unchanged.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use lws_core::{
    account_flags, is_locked, iso_timestamp, now_secs, Account, AccountStatus, Key, Output, OutputRef, Request,
    RequestKind, Spend, Total, ViewKey,
};
use lws_store::{CreateOutcome, NewAccount};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value as Json};
use subtle::ConstantTimeEq;
use tracing::{info, warn};

use crate::daemon_data::Stakes;
use crate::error::{ApiError, ApiResult};
use crate::limits::ClientKey;
use crate::random_outs::{self, MAX_RINGS, MAX_RING_SIZE};
use crate::wire::*;
use crate::{json_bytes, json_response, AppState, ClientAddr, Creation};

fn parse<T: DeserializeOwned>(body: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(body).map_err(|e| ApiError::BadRequest(format!("invalid request: {e}")))
}

fn hex<T: AsRef<[u8]>>(bytes: T) -> String {
    hex::encode(bytes)
}

/// Verifies credentials and loads the account. The view key must equal the
/// stored key (compared in constant time). Every way an account is created
/// checks its key against the address, so this needs no curve operation per
/// request. Hidden accounts are indistinguishable from unknown ones.
async fn open_account(state: &AppState, creds: &Credentials) -> ApiResult<Account> {
    let (view_public, spend_public) = lws_kernel::decode_address(&creds.address, state.config.network)
        .map_err(|_| ApiError::BadRequest("invalid address".into()))?;
    let key = hex32(&creds.view_key).ok_or_else(|| ApiError::BadRequest("invalid view key".into()))?;
    let address = lws_kernel::encode_address(&view_public, &spend_public, state.config.network);
    let account = state.read.account_by_address(&address).await?.ok_or(ApiError::Forbidden)?;
    let same_key: bool = account.view_key.bytes().ct_eq(&key).into();
    if !same_key || account.spend_public != spend_public || account.status == AccountStatus::Hidden {
        return Err(ApiError::Forbidden);
    }
    // At most one write a minute per account, however often the wallet polls.
    let now = now_secs();
    if account.last_access < now - 60 {
        let primary = state.primary.clone();
        let id = account.id;
        tokio::spawn(async move {
            let _ = primary.touch(id, now).await;
        });
    }
    Ok(account)
}

/// Highest block index known: the daemon's, or the scanner's, whichever is further.
async fn blockchain_height(state: &AppState, account: &Account) -> ApiResult<u64> {
    let tip = match state.chain_tip() {
        Some(t) => t,
        None => state.read.tip().await?.map(|b| b.height).unwrap_or(0),
    };
    Ok(tip.max(account.scan_height))
}

/// Runs `render` once per distinct key, sharing the result between
/// concurrent identical requests and for a few seconds after.
async fn cached<F>(state: &AppState, key: String, render: F) -> ApiResult<Response>
where
    F: std::future::Future<Output = ApiResult<Bytes>>,
{
    let bytes = state.responses.try_get_with(key, render).await.map_err(|e| (*e).clone())?;
    Ok(json_response(bytes))
}

fn cache_key(state: &AppState, endpoint: &str, account: &Account, params: impl std::fmt::Display) -> String {
    format!(
        "{endpoint}/{}/{}/{}/{}/{params}",
        account.id,
        account.generation,
        account.scan_height,
        state.chain_tip().unwrap_or(0)
    )
}

#[derive(Serialize)]
struct SpendJson {
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<u64>,
    amount: U64Str,
    key_image: String,
    tx_pub_key: String,
    out_index: u32,
    mixin: u32,
}

fn spend_json(source: &Output, spend: &Spend, with_height: bool) -> SpendJson {
    SpendJson {
        height: with_height.then_some(spend.height),
        amount: U64Str(source.amount),
        key_image: hex(spend.key_image),
        tx_pub_key: hex(source.tx_pub_key),
        out_index: source.out_index,
        mixin: spend.mixin,
    }
}

/// The account's outputs split into BDX and token outputs, indexed by the
/// reference ring members use.
struct Ledger<'a> {
    native: HashMap<OutputRef, &'a Output>,
    tokens: HashMap<OutputRef, &'a Output>,
}

impl<'a> Ledger<'a> {
    fn new(outputs: &'a [Output]) -> Self {
        let mut native = HashMap::with_capacity(outputs.len());
        let mut tokens = HashMap::new();
        for o in outputs {
            if o.token.is_some() {
                tokens.insert(o.source, o);
            } else {
                native.insert(o.source, o);
            }
        }
        Ledger { native, tokens }
    }
}

/// Stake-locked and time-locked BDX, as the reference computes `locked_funds`.
fn locked_funds<'a>(outputs: impl Iterator<Item = &'a Output>, stakes: &Stakes, scan_height: u64) -> u64 {
    let now = now_secs();
    let mut processed: HashSet<Key> = HashSet::new();
    let mut total = Total::default();
    for o in outputs.filter(|o| o.token.is_none()) {
        if let Some(image) = o.locked_key_image.filter(|k| *k != [0u8; 32]) {
            if !processed.contains(&image) {
                if let Some(&amount) = stakes.blacklisted.get(&image).or_else(|| stakes.locked.get(&image)) {
                    total.add(amount);
                    processed.insert(image);
                }
            }
        }
        if is_locked(o.unlock_time, scan_height, now) {
            total.add(o.amount);
        }
    }
    total.get()
}

#[derive(Serialize)]
struct TokenBalanceJson {
    token_id: String,
    total_received: U64Str,
    total_sent: U64Str,
    locked_funds: U64Str,
}

#[derive(Serialize)]
struct AddressInfoJson {
    locked_funds: U64Str,
    total_received: U64Str,
    total_sent: U64Str,
    scanned_height: u64,
    scanned_block_height: u64,
    start_height: u64,
    transaction_height: u64,
    blockchain_height: u64,
    next_min_height: u64,
    spent_outputs: Vec<SpendJson>,
    tokens: Vec<TokenBalanceJson>,
}

pub async fn get_address_info(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let req: HistoryRequest = parse(&body)?;
    let account = open_account(&state, &req.creds).await?;
    let (min_height, max_count) = (req.min_height.unwrap_or(0), req.max_count.unwrap_or(0));
    let key = cache_key(&state, "info", &account, format_args!("{min_height}/{max_count}"));
    let st = state.clone();
    cached(&state, key, async move {
        let stakes = st.daemon.stakes().await?;
        let outputs = st.read.outputs(account.id, 0).await?;
        let spends = st.read.spends(account.id, min_height).await?;
        let ledger = Ledger::new(&outputs);
        let height = blockchain_height(&st, &account).await?;
        let incremental = min_height != 0 || max_count != 0;

        let mut total_received = Total::default();
        outputs.iter().filter(|o| o.token.is_none()).for_each(|o| total_received.add(o.amount));

        let mut total_sent = Total::default();
        let mut token_sent: HashMap<Key, Total> = HashMap::new();
        let mut spent_outputs = Vec::new();
        let mut next_min_height = 0;
        let (mut returned, mut last_height) = (0u64, 0u64);
        for spend in &spends {
            if max_count != 0 && returned >= max_count && spend.height != last_height {
                next_min_height = spend.height;
                break;
            }
            if let Some(source) = ledger.native.get(&spend.source) {
                spent_outputs.push(spend_json(source, spend, incremental));
                total_sent.add(source.amount);
            } else if let Some(source) = ledger.tokens.get(&spend.source) {
                token_sent.entry(*source.token_id().unwrap()).or_default().add(source.amount);
            } else {
                warn!(account = account.id, "spend without a matching receive; skipped");
                continue;
            }
            returned += 1;
            last_height = spend.height;
        }

        let now = now_secs();
        let mut tokens: Vec<TokenBalanceJson> = Vec::new();
        let mut seen: Vec<Key> = Vec::new();
        for o in outputs.iter().filter(|o| o.token.is_some()) {
            let id = *o.token_id().unwrap();
            if !seen.contains(&id) {
                seen.push(id);
            }
        }
        for id in seen {
            let mut received = Total::default();
            let mut locked = Total::default();
            for o in outputs.iter().filter(|o| o.token_id() == Some(&id)) {
                received.add(o.amount);
                if is_locked(o.unlock_time, account.scan_height, now) {
                    locked.add(o.amount);
                }
            }
            tokens.push(TokenBalanceJson {
                token_id: hex(id),
                total_received: U64Str(received.get()),
                total_sent: U64Str(token_sent.get(&id).copied().unwrap_or_default().get()),
                locked_funds: U64Str(locked.get()),
            });
        }

        json_bytes(
            &st,
            &AddressInfoJson {
                locked_funds: U64Str(locked_funds(outputs.iter(), &stakes, account.scan_height)),
                total_received: U64Str(total_received.get()),
                total_sent: U64Str(total_sent.get()),
                scanned_height: account.scan_height,
                scanned_block_height: account.scan_height,
                start_height: account.start_height,
                transaction_height: height,
                blockchain_height: height,
                next_min_height,
                spent_outputs,
                tokens,
            },
        )
    })
    .await
}

#[derive(Serialize)]
struct TokenLegJson {
    token_id: String,
    received: U64Str,
    sent: U64Str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    spent_outputs: Vec<SpendJson>,
}

#[derive(Serialize)]
struct TxJson {
    id: u64,
    hash: String,
    timestamp: String,
    total_received: U64Str,
    total_sent: U64Str,
    unlock_time: u64,
    height: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    payment_id: Option<String>,
    coinbase: bool,
    mempool: bool,
    mixin: u32,
    spent_outputs: Vec<SpendJson>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    token_legs: Vec<TokenLegJson>,
}

struct TxAcc {
    hash: Key,
    height: u64,
    timestamp: u64,
    unlock_time: u64,
    mixin: u32,
    payment_id: Option<Vec<u8>>,
    coinbase: bool,
    received: Total,
    sent: Total,
    spends: Vec<SpendJson>,
    legs: Vec<(Key, Total, Total, Vec<SpendJson>)>,
}

impl TxAcc {
    fn from_output(o: &Output, received: u64) -> Self {
        let mut r = Total::default();
        r.add(received);
        TxAcc {
            hash: o.tx_hash,
            height: o.height,
            timestamp: o.timestamp,
            unlock_time: o.unlock_time,
            mixin: o.mixin,
            payment_id: o.payment_id.clone(),
            coinbase: o.is_coinbase(),
            received: r,
            sent: Total::default(),
            spends: Vec::new(),
            legs: Vec::new(),
        }
    }
    fn from_spend(s: &Spend) -> Self {
        TxAcc {
            hash: s.tx_hash,
            height: s.height,
            timestamp: s.timestamp,
            unlock_time: s.unlock_time,
            mixin: s.mixin,
            payment_id: None,
            coinbase: false,
            received: Total::default(),
            sent: Total::default(),
            spends: Vec::new(),
            legs: Vec::new(),
        }
    }
    fn leg(&mut self, id: Key) -> &mut (Key, Total, Total, Vec<SpendJson>) {
        if let Some(i) = self.legs.iter().position(|l| l.0 == id) {
            return &mut self.legs[i];
        }
        self.legs.push((id, Total::default(), Total::default(), Vec::new()));
        self.legs.last_mut().unwrap()
    }
}

#[derive(Serialize)]
struct AddressTxsJson {
    total_received: U64Str,
    locked_funds: U64Str,
    scanned_height: u64,
    scanned_block_height: u64,
    start_height: u64,
    transaction_height: u64,
    blockchain_height: u64,
    next_min_height: u64,
    transactions: Vec<TxJson>,
}

pub async fn get_address_txs(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let req: HistoryRequest = parse(&body)?;
    let account = open_account(&state, &req.creds).await?;
    let (min_height, max_count) = (req.min_height.unwrap_or(0), req.max_count.unwrap_or(0));
    let key = cache_key(&state, "txs", &account, format_args!("{min_height}/{max_count}"));
    let st = state.clone();
    cached(&state, key, async move {
        let stakes = st.daemon.stakes().await?;
        let outputs = st.read.outputs(account.id, 0).await?;
        let spends = st.read.spends(account.id, min_height).await?;
        let height = blockchain_height(&st, &account).await?;
        let incremental = min_height != 0 || max_count != 0;

        // Merge receives and spends in chain order, one entry per tx.
        let mut native: HashMap<OutputRef, &Output> = HashMap::new();
        let mut tokens: HashMap<OutputRef, &Output> = HashMap::new();
        let mut txs: Vec<TxAcc> = Vec::new();
        let mut total_received = Total::default();
        let (mut oi, mut si) = (0, 0);
        while oi < outputs.len() || si < spends.len() {
            let take_output = match (outputs.get(oi), spends.get(si)) {
                (Some(o), Some(s)) => (o.height, o.tx_hash) <= (s.height, s.tx_hash),
                (Some(_), None) => true,
                _ => false,
            };
            if take_output {
                let o = &outputs[oi];
                oi += 1;
                let same = txs.last().is_some_and(|t| t.hash == o.tx_hash);
                if let Some(id) = o.token_id().copied() {
                    tokens.insert(o.source, o);
                    if !same {
                        txs.push(TxAcc::from_output(o, 0));
                    }
                    txs.last_mut().unwrap().leg(id).1.add(o.amount);
                } else {
                    native.insert(o.source, o);
                    total_received.add(o.amount);
                    if same {
                        txs.last_mut().unwrap().received.add(o.amount);
                    } else {
                        txs.push(TxAcc::from_output(o, o.amount));
                    }
                }
            } else {
                let s = &spends[si];
                si += 1;
                let same = txs.last().is_some_and(|t| t.hash == s.tx_hash);
                if let Some(source) = native.get(&s.source) {
                    if !same {
                        txs.push(TxAcc::from_spend(s));
                    }
                    let tx = txs.last_mut().unwrap();
                    tx.spends.push(spend_json(source, s, incremental));
                    tx.sent.add(source.amount);
                } else if let Some(source) = tokens.get(&s.source) {
                    if !same {
                        txs.push(TxAcc::from_spend(s));
                    }
                    let leg = txs.last_mut().unwrap().leg(*source.token_id().unwrap());
                    leg.2.add(source.amount);
                    leg.3.push(spend_json(source, s, incremental));
                } else {
                    warn!(account = account.id, "spend without a matching receive; skipped");
                }
            }
        }

        txs.retain(|t| t.height >= min_height);
        let mut next_min_height = 0;
        if max_count != 0 && txs.len() as u64 > max_count {
            let mut cut = max_count as usize;
            let boundary = txs[cut - 1].height;
            while cut < txs.len() && txs[cut].height == boundary {
                cut += 1;
            }
            if cut < txs.len() {
                next_min_height = txs[cut].height;
                txs.truncate(cut);
            }
        }

        let transactions = txs
            .into_iter()
            .enumerate()
            .map(|(i, t)| TxJson {
                id: i as u64,
                hash: hex(t.hash),
                timestamp: iso_timestamp(t.timestamp),
                total_received: U64Str(t.received.get()),
                total_sent: U64Str(t.sent.get()),
                unlock_time: t.unlock_time,
                height: t.height,
                payment_id: t.payment_id.as_ref().map(hex),
                coinbase: t.coinbase,
                mempool: false,
                mixin: t.mixin,
                spent_outputs: t.spends,
                token_legs: t
                    .legs
                    .into_iter()
                    .map(|(id, r, s, spends)| TokenLegJson {
                        token_id: hex(id),
                        received: U64Str(r.get()),
                        sent: U64Str(s.get()),
                        spent_outputs: spends,
                    })
                    .collect(),
            })
            .collect();

        json_bytes(
            &st,
            &AddressTxsJson {
                total_received: U64Str(total_received.get()),
                locked_funds: U64Str(locked_funds(outputs.iter(), &stakes, account.scan_height)),
                scanned_height: account.scan_height,
                scanned_block_height: account.scan_height,
                start_height: account.start_height,
                transaction_height: height,
                blockchain_height: height,
                next_min_height,
                transactions,
            },
        )
    })
    .await
}

#[derive(Serialize)]
struct UnspentJson {
    amount: U64Str,
    public_key: String,
    index: u32,
    global_index: u64,
    tx_id: u64,
    tx_hash: String,
    tx_prefix_hash: String,
    tx_pub_key: String,
    timestamp: String,
    height: u64,
    spend_key_images: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rct: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    blinded_token_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount_commitment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encrypted_amount: Option<U64Str>,
}

#[derive(Serialize)]
struct UnspentOutsJson {
    fee_per_byte: u64,
    fee_per_output: u64,
    flash_fee_per_byte: u64,
    flash_fee_per_output: u64,
    flash_fee_fixed: u64,
    quantization_mask: u64,
    fork_version: u64,
    amount: U64Str,
    next_min_height: u64,
    blockchain_height: u64,
    outputs: Vec<UnspentJson>,
}

fn rct_of(o: &Output, view_key: &ViewKey) -> ApiResult<Option<String>> {
    if o.flags & (lws_core::output_flags::COINBASE | lws_core::output_flags::RINGCT) == 0 {
        return Ok(None);
    }
    if let Some(r) = o.rct.as_ref().filter(|r| r.len() == 96) {
        return Ok(Some(hex(r)));
    }
    let r = lws_kernel::output_rct(&o.tx_pub_key, view_key.bytes(), o.out_index, o.amount, &o.mask)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(Some(hex(r)))
}

pub async fn get_unspent_outs(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let req: UnspentRequest = parse(&body)?;
    let account = open_account(&state, &req.creds).await?;
    let min_height = req.min_height.unwrap_or(0);
    let max_count = req.max_count.unwrap_or(0);
    let dust = if req.use_dust.unwrap_or(false) { 0 } else { req.dust_threshold.map_or(0, |d| d.0) };
    let mixin = req.mixin.unwrap_or(0);
    let wanted_token = req.token_id.as_deref().filter(|t| is_token_id(t)).and_then(hex32);
    let all_tokens = req.all_tokens.unwrap_or(false);
    let key = cache_key(
        &state,
        "unspent",
        &account,
        format_args!("{}/{min_height}/{max_count}/{dust}/{mixin}/{wanted_token:?}/{all_tokens}", req.amount.0),
    );
    let st = state.clone();
    cached(&state, key, async move {
        let stakes = st.daemon.stakes().await?;
        let fees = st.daemon.fees().await?;
        let fork_version = st.daemon.fork_version().await?;
        let outputs = st.read.outputs(account.id, min_height).await?;
        let spends = st.read.spends(account.id, 0).await?;
        let mut images: HashMap<OutputRef, Vec<Key>> = HashMap::new();
        for s in &spends {
            images.entry(s.source).or_default().push(s.key_image);
        }

        let now = now_secs();
        let mut received = Total::default();
        let mut out = Vec::new();
        let mut next_min_height = 0;
        let (mut returned, mut last_height) = (0u64, 0u64);
        for o in &outputs {
            if max_count != 0 && returned >= max_count && o.height != last_height {
                next_min_height = o.height;
                break;
            }
            if let Some(id) = o.token_id() {
                if !all_tokens && Some(*id) != wanted_token {
                    continue;
                }
            }
            if is_locked(o.unlock_time, account.scan_height, now) {
                continue;
            }
            if o.amount < dust || (o.mixin < mixin && !o.is_coinbase()) {
                continue;
            }
            if let Some(image) = o.locked_key_image {
                let staked = stakes.blacklisted.get(&image).or_else(|| stakes.locked.get(&image));
                if staked == Some(&o.amount) {
                    continue;
                }
            }
            if o.token.is_none() {
                received.add(o.amount);
            }
            let token = o.token.as_ref();
            out.push(UnspentJson {
                amount: U64Str(o.amount),
                public_key: hex(o.pub_key),
                index: o.out_index,
                global_index: o.source.global_index,
                tx_id: o.source.global_index,
                tx_hash: hex(o.tx_hash),
                tx_prefix_hash: hex(o.tx_prefix_hash),
                tx_pub_key: hex(o.tx_pub_key),
                timestamp: iso_timestamp(o.timestamp),
                height: o.height,
                spend_key_images: images.get(&o.source).map(|v| v.iter().map(hex).collect()).unwrap_or_default(),
                rct: rct_of(o, &account.view_key)?,
                token_id: token.map(|t| hex(t.token_id)),
                blinded_token_id: token.map(|t| hex(t.blinded_token_id)),
                amount_commitment: token.map(|t| hex(t.amount_commitment)),
                encrypted_amount: token.map(|t| U64Str(t.encrypted_amount)),
            });
            returned += 1;
            last_height = o.height;
        }
        if min_height == 0 && received.exact() < u128::from(req.amount.0) {
            // The reference answers an unaffordable amount with 403.
            return Err(ApiError::Forbidden);
        }
        json_bytes(
            &st,
            &UnspentOutsJson {
                fee_per_byte: fees.fee_per_byte,
                fee_per_output: fees.fee_per_output,
                flash_fee_per_byte: fees.flash_fee_per_byte,
                flash_fee_per_output: fees.flash_fee_per_output,
                flash_fee_fixed: fees.flash_fee_fixed,
                quantization_mask: fees.quantization_mask,
                fork_version,
                amount: U64Str(received.get()),
                next_min_height,
                blockchain_height: blockchain_height(&st, &account).await?,
                outputs: out,
            },
        )
    })
    .await
}

#[derive(Serialize)]
struct DecoyJson {
    global_index: U64Str,
    public_key: String,
    rct: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blinded_token_id: Option<String>,
}

#[derive(Serialize)]
struct RingJson {
    amount: U64Str,
    outputs: Vec<DecoyJson>,
}

pub async fn get_random_outs(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let req: RandomOutsRequest = parse(&body)?;
    if req.count > MAX_RING_SIZE || req.amounts.len() > MAX_RINGS {
        return Err(ApiError::BadRequest(format!("at most {MAX_RINGS} rings of {MAX_RING_SIZE} decoys")));
    }
    let requests = req
        .amounts
        .iter()
        .enumerate()
        .map(|(i, a)| (a.0, req.token_ids.get(i).is_some_and(|t| is_token_id(t))))
        .collect();
    let rings = random_outs::pick(&state.daemon, req.count, requests).await?;
    let amount_outs: Vec<RingJson> = rings
        .into_iter()
        .map(|r| RingJson {
            amount: U64Str(r.amount),
            outputs: r
                .outputs
                .into_iter()
                .map(|o| {
                    let mut rct = [0u8; 96];
                    rct[..32].copy_from_slice(&o.mask);
                    DecoyJson {
                        global_index: U64Str(o.global_index),
                        public_key: hex(o.key),
                        rct: hex(rct),
                        blinded_token_id: o.blinded_token_id.map(hex),
                    }
                })
                .collect(),
        })
        .collect();
    Ok(json_response(json_bytes(&state, &json!({ "amount_outs": amount_outs }))?))
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    Extension(ClientAddr(ip)): Extension<ClientAddr>,
    body: Bytes,
) -> ApiResult<Response> {
    let req: LoginRequest = parse(&body)?;
    let network = state.config.network;
    let (view_public, spend_public) = lws_kernel::decode_address(&req.creds.address, network)
        .map_err(|_| ApiError::BadRequest("invalid address".into()))?;
    let key = hex32(&req.creds.view_key).ok_or_else(|| ApiError::BadRequest("invalid view key".into()))?;
    if !lws_kernel::view_key_matches(&key, &view_public) {
        return Err(ApiError::Forbidden);
    }
    let address = lws_kernel::encode_address(&view_public, &spend_public, network);

    let respond = |new_address: bool, generated_locally: bool, start_height: u64| -> ApiResult<Response> {
        Ok(json_response(json_bytes(
            &state,
            &json!({"new_address": new_address, "generated_locally": generated_locally, "start_height": start_height}),
        )?))
    };

    if let Some(existing) = state.primary.account_by_address(&address).await? {
        let same_key: bool = existing.view_key.bytes().ct_eq(&key).into();
        if !same_key || existing.status == AccountStatus::Hidden {
            return Err(ApiError::Forbidden);
        }
        return respond(false, existing.generated_locally(), existing.start_height);
    }
    if !req.create_account {
        return Err(ApiError::Forbidden);
    }
    if state.config.creation == Creation::Disabled {
        return Err(ApiError::NotImplemented);
    }
    if let Err(retry_after) = state.creation_limiter.check(ClientKey::from(ip), 1.0) {
        lws_core::metrics::Metrics::add(&state.metrics.http_rate_limited_total, 1);
        return Err(ApiError::RateLimited { retry_after });
    }

    let start_height = match state.chain_tip() {
        Some(t) => t,
        None => state.daemon.daemon.get_info().await?.height.saturating_sub(1),
    };
    let flags = if req.generated_locally { account_flags::GENERATED_LOCALLY } else { 0 };

    if state.config.creation == Creation::Manual {
        let request = Request {
            kind: RequestKind::Create,
            address: address.clone(),
            view_public,
            spend_public,
            view_key: ViewKey::new(key),
            start_height,
            flags,
            created_at: now_secs(),
        };
        state.primary.add_request(&request).await?;
        return respond(true, req.generated_locally, start_height);
    }

    let new = NewAccount {
        address,
        view_public,
        spend_public,
        view_key: ViewKey::new(key),
        status: AccountStatus::Active,
        flags,
        start_height,
    };
    match state.primary.create_account(&new).await? {
        CreateOutcome::Created(a) => {
            lws_core::metrics::Metrics::add(&state.metrics.accounts_created_total, 1);
            info!(account = a.id, start_height, "account created");
            respond(true, req.generated_locally, a.start_height)
        }
        CreateOutcome::Exists(a) => respond(false, a.generated_locally(), a.start_height),
    }
}

pub async fn import_request(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let creds: Credentials = parse(&body)?;
    let account = open_account(&state, &creds).await?;
    let (status, new_request, fulfilled) = if account.start_height == 0 {
        ("Approved", false, true)
    } else if state.primary.get_request(RequestKind::ImportScan, &account.address).await?.is_some() {
        ("Waiting for Approval", false, false)
    } else {
        let request = Request {
            kind: RequestKind::ImportScan,
            address: account.address.clone(),
            view_public: account.view_public,
            spend_public: account.spend_public,
            view_key: account.view_key.clone(),
            start_height: 0,
            flags: 0,
            created_at: now_secs(),
        };
        state.primary.add_request(&request).await?;
        if state.config.auto_accept_imports {
            state.primary.accept_requests(RequestKind::ImportScan, std::slice::from_ref(&account.address)).await?;
            ("Approved", true, true)
        } else {
            ("Accepted, waiting for approval", true, false)
        }
    };
    Ok(json_response(json_bytes(
        &state,
        &json!({"import_fee": U64Str(0), "status": status, "new_request": new_request, "request_fulfilled": fulfilled}),
    )?))
}

/// beldexd's `reason_codes`, most specific first, with what to tell the user.
/// `not_relayed` comes with every rejection, so it only counts alone.
const REJECTIONS: &[(&str, &str)] = &[
    ("double_spend", "Transaction rejected: these funds are already spent. Refresh the wallet and try again."),
    ("overspend", "Transaction rejected: it spends more than its inputs hold."),
    ("fee_too_low", "Transaction rejected: the fee is too low."),
    ("mixin", "Transaction rejected: it has the wrong number of decoys."),
    ("invalid_input", "Transaction rejected: one or more inputs are invalid."),
    ("invalid_output", "Transaction rejected: one or more outputs are invalid."),
    ("too_few_outputs", "Transaction rejected: it has too few outputs."),
    ("too_big", "Transaction rejected: it is too large."),
    ("invalid_version", "Transaction rejected: its version is not valid now. The wallet may need an update."),
    ("invalid_type", "Transaction rejected: its type is not valid."),
    ("mnode_locked", "Transaction rejected: these funds are staked to a master node."),
    ("blacklisted", "Transaction rejected: these funds are in a master node penalty period."),
    ("altchain", "Transaction rejected: it spends outputs on another chain. Refresh the wallet and try again."),
    ("failed", "Transaction rejected: it failed verification on the Beldex node."),
];

/// Longest daemon text passed on to a wallet, which may show it as is.
const MAX_REASON_CHARS: usize = 500;

/// A failed submission. Always a non-200: wallets take any 200 as sent without
/// reading the body. `Error` is the field the `@bdxi` web and mobile clients
/// show, `error` the one beldex core's light wallet reads, and `may_be_sent`
/// says the transaction could still reach the network, so sending it again
/// could pay twice unless the wallet checks first.
fn submit_failure(
    http: StatusCode,
    code: &str,
    message: &str,
    may_be_sent: bool,
    details: Option<(&str, &Json)>,
) -> Response {
    let mut body =
        json!({"status": "FAILED", "code": code, "Error": message, "error": message, "may_be_sent": may_be_sent});
    if let Some((reason, result)) = details {
        body["reason"] = reason.into();
        for field in ["reason_codes", "flash_status"] {
            if let Some(value) = result.get(field) {
                body[field] = value.clone();
            }
        }
    }
    (http, axum::Json(body)).into_response()
}

/// Turns beldexd's verdict on a transaction it did not accept into the reply.
fn submit_rejected(result: &Json) -> Response {
    let reason: String =
        result.get("reason").and_then(Json::as_str).unwrap_or_default().trim().chars().take(MAX_REASON_CHARS).collect();
    let codes: Vec<&str> =
        result.get("reason_codes").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str).collect();
    warn!(%reason, ?codes, status = ?result.get("status"), "daemon rejected submitted transaction");

    let reply = |code, message: &str, may_be_sent| {
        submit_failure(StatusCode::INTERNAL_SERVER_ERROR, code, message, may_be_sent, Some((&reason, result)))
    };
    if let Some((code, message)) = REJECTIONS.iter().find(|(code, _)| codes.contains(code)) {
        return reply(code, message, false);
    }
    // beldexd's flash_result: 1 rejected, 3 timeout. A timed-out flash can still
    // have reached the network.
    match result.get("flash_status").and_then(Json::as_u64) {
        Some(3) => {
            return reply(
                "flash_timeout",
                "The flash quorum did not answer in time. The transaction may still go through, so check the wallet before sending again.",
                true,
            )
        }
        Some(1) => return reply("flash_rejected", "Transaction rejected by the flash quorum.", false),
        _ => {}
    }
    match result.get("status").and_then(Json::as_str) {
        Some("BUSY") => reply("daemon_busy", "The Beldex node is still syncing. Try again shortly.", false),
        Some("FAILED") if codes == ["not_relayed"] => reply(
            "not_relayed",
            "The Beldex node did not relay this transaction, possibly because it already has it. Check the wallet before sending again.",
            true,
        ),
        Some("FAILED") if !reason.is_empty() => reply("rejected", &format!("Transaction rejected: {reason}"), false),
        Some("FAILED") => reply("rejected", "Transaction rejected by the Beldex node.", false),
        _ => reply(
            "bad_daemon_response",
            "The Beldex node gave an unexpected answer. Check the wallet before sending again.",
            true,
        ),
    }
}

pub async fn submit_raw_tx(State(state): State<Arc<AppState>>, body: Bytes) -> Response {
    let invalid = |message: &str| submit_failure(StatusCode::BAD_REQUEST, "invalid_request", message, false, None);
    let req: SubmitRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => return invalid(&format!("invalid request: {e}")),
    };
    if req.tx.is_empty() || !req.tx.len().is_multiple_of(2) || !req.tx.bytes().all(|b| b.is_ascii_hexdigit()) {
        return invalid("tx must be hex");
    }
    let flash = req.fee.as_deref() == Some("5");
    let result = match state.daemon.daemon.send_raw_transaction(&req.tx, flash).await {
        Ok(result) => result,
        // The daemon refused the call itself, so the transaction went nowhere.
        Err(e @ lws_daemon::Error::Rpc { .. }) => {
            warn!(error = %e, "daemon refused submit_raw_tx");
            let message = "The Beldex node could not process this transaction.";
            return submit_failure(StatusCode::SERVICE_UNAVAILABLE, "daemon_error", message, false, None);
        }
        // Includes a timeout after the daemon took the transaction. The detail
        // names daemon addresses, so it stays in the log.
        Err(e) => {
            warn!(error = %e, "submit_raw_tx: no answer from daemon");
            let message = "No answer from the Beldex node, so this transaction may or may not have been sent. Check the wallet before sending again.";
            return submit_failure(StatusCode::SERVICE_UNAVAILABLE, "daemon_unavailable", message, true, None);
        }
    };
    if result.get("status").and_then(Json::as_str) != Some("OK") {
        return submit_rejected(&result);
    }
    (StatusCode::OK, axum::Json(json!({"status": "OK"}))).into_response()
}

pub async fn daemon_status(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    let body = match state.daemon.daemon.get_info().await {
        Ok(info) => {
            let network = match info.nettype.as_str() {
                "mainnet" => "main",
                "testnet" => "test",
                "devnet" => "stage",
                _ => "fake",
            };
            let state_name = if info.outgoing_connections == 0 && info.incoming_connections == 0 {
                "no_connections"
            } else if info.target_height > info.height && info.target_height - info.height >= 5 {
                "synchronizing"
            } else {
                "ok"
            };
            json!({
                "outgoing_connections_count": info.outgoing_connections,
                "incoming_connections_count": info.incoming_connections,
                "height": info.height,
                "target_height": info.target_height,
                "network": network,
                "state": state_name,
            })
        }
        Err(e) => {
            warn!(error = %e, "daemon_status: daemon unavailable");
            json!({"outgoing_connections_count": 0, "incoming_connections_count": 0, "height": 0,
                   "target_height": 0, "network": "main", "state": "unavailable"})
        }
    };
    Ok(json_response(json_bytes(&state, &body)?))
}

#[derive(Serialize, Default)]
struct TokenEntryJson {
    token_id: String,
    status: String,
    total_received: U64Str,
    total_sent: U64Str,
    locked_funds: U64Str,
    unlocked_balance: U64Str,
    ticker: String,
    full_name: String,
    owner: String,
    meta_info: String,
    current_supply: U64Str,
    total_max_supply: U64Str,
    decimal_point: u32,
}

pub async fn get_token_balances(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let req: TokenBalancesRequest = parse(&body)?;
    if req.token_ids.len() > 100 {
        return Err(ApiError::BadRequest("at most 100 token_ids".into()));
    }
    let account = open_account(&state, &req.creds).await?;
    let mut extra: Vec<String> =
        req.token_ids.iter().filter(|t| is_token_id(t)).map(|t| t.to_ascii_lowercase()).collect();
    extra.sort();
    extra.dedup();
    let key = cache_key(&state, "tokens", &account, extra.join(","));
    let st = state.clone();
    cached(&state, key, async move {
        let outputs = st.read.outputs(account.id, 0).await?;
        let spends = st.read.spends(account.id, 0).await?;
        let ledger = Ledger::new(&outputs);
        let now = now_secs();

        let mut ids: Vec<String> = Vec::new();
        for o in outputs.iter().filter_map(|o| o.token_id()) {
            let id = hex(o);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        for id in extra {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }

        let mut tokens = Vec::with_capacity(ids.len());
        for id in ids {
            let raw = hex32(&id).unwrap_or_default();
            let (mut received, mut locked, mut sent) = (Total::default(), Total::default(), Total::default());
            for o in outputs.iter().filter(|o| o.token_id() == Some(&raw)) {
                received.add(o.amount);
                if is_locked(o.unlock_time, account.scan_height, now) {
                    locked.add(o.amount);
                }
            }
            for s in &spends {
                if let Some(o) = ledger.tokens.get(&s.source).filter(|o| o.token_id() == Some(&raw)) {
                    sent.add(o.amount);
                }
            }
            // Exact arithmetic first: the totals can exceed 64 bits on chains
            // with very large token supplies even when the balance does not.
            let unlocked = received.exact().saturating_sub(sent.exact() + locked.exact());
            let mut entry = TokenEntryJson {
                token_id: id.clone(),
                total_received: U64Str(received.get()),
                total_sent: U64Str(sent.get()),
                locked_funds: U64Str(locked.get()),
                unlocked_balance: U64Str(u64::try_from(unlocked).unwrap_or(u64::MAX)),
                ..Default::default()
            };
            match st.daemon.token(&id).await {
                Err(_) => entry.status = "unknown".into(),
                Ok(None) => entry.status = "not_found".into(),
                Ok(Some(info)) => {
                    entry.status = "confirmed".into();
                    entry.ticker = info.ticker.clone();
                    entry.full_name = info.full_name.clone();
                    entry.owner = info.owner.clone();
                    entry.meta_info = info.meta_info.clone();
                    entry.current_supply = U64Str(info.current_supply);
                    entry.total_max_supply = U64Str(info.total_max_supply);
                    entry.decimal_point = info.decimal_point;
                }
            }
            tokens.push(entry);
        }
        let tip = st.chain_tip().unwrap_or(0);
        json_bytes(&st, &json!({"tokens": tokens, "scanned_height": account.scan_height, "blockchain_height": tip}))
    })
    .await
}

pub async fn get_token_info(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let req: TokenInfoRequest = parse(&body)?;
    if !is_token_id(&req.token_id) {
        return Err(ApiError::BadRequest("token_id must be 64 hex characters".into()));
    }
    let info = state
        .daemon
        .token(&req.token_id.to_ascii_lowercase())
        .await?
        .ok_or_else(|| ApiError::BadRequest("unknown token".into()))?;
    Ok(json_response(json_bytes(
        &state,
        &json!({
            "token_id": info.token_id,
            "ticker": info.ticker,
            "full_name": info.full_name,
            "owner": info.owner,
            "meta_info": info.meta_info,
            "current_supply": U64Str(info.current_supply),
            "total_max_supply": U64Str(info.total_max_supply),
            "decimal_point": info.decimal_point,
        }),
    )?))
}

pub async fn get_token_list(State(state): State<Arc<AppState>>, body: Bytes) -> ApiResult<Response> {
    let req: TokenListRequest = if body.is_empty() { serde_json::from_str("{}").unwrap() } else { parse(&body)? };
    if req.count > 1000 {
        return Err(ApiError::BadRequest("count may not exceed 1000".into()));
    }
    let r =
        match state.daemon.daemon.json_rpc("get_token_list", json!({"offset": req.offset, "count": req.count})).await {
            Ok(r) => r,
            Err(lws_daemon::Error::Rpc { .. }) => return Err(ApiError::BadRequest("token list unavailable".into())),
            Err(e) => return Err(e.into()),
        };
    let ids: Vec<String> = r
        .get("token_ids")
        .and_then(Json::as_array)
        .map(|a| a.iter().filter_map(Json::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let total = r.get("total_count").and_then(Json::as_u64).unwrap_or(ids.len() as u64);
    Ok(json_response(json_bytes(&state, &json!({"token_ids": ids, "total_count": total}))?))
}

pub async fn health(State(state): State<Arc<AppState>>) -> ApiResult<Response> {
    state.read.ping().await.map_err(|_| ApiError::Unavailable("database".into()))?;
    Ok(json_response(Bytes::from_static(br#"{"status":"ok"}"#)))
}

#[cfg(test)]
pub(crate) fn tests_locked(outputs: &[Output], stakes: &Stakes, scan_height: u64) -> u64 {
    locked_funds(outputs.iter(), stakes, scan_height)
}
