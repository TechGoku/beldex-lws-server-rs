//! Daemon state the API needs on every request: master-node stakes, fee
//! estimates, the fork version, the output distribution and token descriptors.
//!
//! Each is cached with a TTL and refreshed by a single caller while everyone
//! else keeps the cached value (no thundering herd on the daemon). If a
//! refresh fails, a recent value is served for a grace period instead of
//! failing every wallet request while beldexd restarts.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lws_core::Key;
use lws_daemon::Daemon;
use serde_json::{json, Value as Json};
use tracing::warn;

use crate::error::{ApiError, ApiResult};
use crate::wire::hex32;

pub struct Cached<T> {
    ttl: Duration,
    max_stale: Duration,
    value: Mutex<Option<(Instant, Arc<T>)>>,
    refresh: tokio::sync::Mutex<()>,
}

impl<T> Cached<T> {
    pub fn new(ttl: Duration, max_stale: Duration) -> Self {
        Cached { ttl, max_stale, value: Mutex::new(None), refresh: tokio::sync::Mutex::new(()) }
    }

    fn fresh(&self) -> Option<Arc<T>> {
        let guard = self.value.lock().unwrap();
        guard.as_ref().filter(|(at, _)| at.elapsed() < self.ttl).map(|(_, v)| v.clone())
    }

    pub async fn get<F, Fut>(&self, fetch: F) -> ApiResult<Arc<T>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ApiResult<T>>,
    {
        if let Some(v) = self.fresh() {
            return Ok(v);
        }
        let _single = self.refresh.lock().await;
        if let Some(v) = self.fresh() {
            return Ok(v);
        }
        match fetch().await {
            Ok(v) => {
                let v = Arc::new(v);
                *self.value.lock().unwrap() = Some((Instant::now(), v.clone()));
                Ok(v)
            }
            Err(e) => {
                let stale = self.value.lock().unwrap().clone();
                match stale {
                    Some((at, v)) if at.elapsed() < self.ttl + self.max_stale => {
                        warn!(error = %e, age_s = at.elapsed().as_secs(), "daemon refresh failed, serving cached value");
                        Ok(v)
                    }
                    _ => Err(e),
                }
            }
        }
    }
}

#[derive(Default)]
pub struct Stakes {
    /// Key image of a blacklisted stake -> amount.
    pub blacklisted: HashMap<Key, u64>,
    /// Key image of a locked master-node contribution -> amount.
    pub locked: HashMap<Key, u64>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Fees {
    pub fee_per_byte: u64,
    pub fee_per_output: u64,
    pub flash_fee_per_byte: u64,
    pub flash_fee_per_output: u64,
    pub flash_fee_fixed: u64,
    pub quantization_mask: u64,
}

#[derive(Default)]
pub struct Distribution {
    /// Cumulative ringct output counts per block.
    pub native: Vec<u64>,
    /// Cumulative token output counts per block, and the global index of
    /// each token output by rank.
    pub token: Option<(Vec<u64>, Vec<u64>)>,
}

#[derive(Clone, Debug, Default)]
pub struct TokenInfo {
    pub token_id: String,
    pub ticker: String,
    pub full_name: String,
    pub owner: String,
    pub meta_info: String,
    pub current_supply: u64,
    pub total_max_supply: u64,
    pub decimal_point: u32,
}

pub struct DaemonData {
    pub daemon: Arc<Daemon>,
    stakes: Cached<Stakes>,
    fees: Cached<Fees>,
    fork: Cached<u64>,
    distribution: Cached<Distribution>,
    /// `None` means the daemon says the token does not exist.
    tokens: moka::future::Cache<String, Option<Arc<TokenInfo>>>,
}

fn u64_of(v: &Json, key: &str) -> ApiResult<u64> {
    v.get(key).and_then(Json::as_u64).ok_or_else(|| ApiError::Unavailable(format!("daemon response lacks '{key}'")))
}

fn str_of(v: &Json, key: &str) -> String {
    v.get(key).and_then(Json::as_str).unwrap_or_default().to_owned()
}

fn arr<'a>(v: &'a Json, key: &str) -> impl Iterator<Item = &'a Json> {
    v.get(key).and_then(Json::as_array).into_iter().flatten()
}

impl DaemonData {
    pub fn new(daemon: Arc<Daemon>) -> Self {
        let grace = Duration::from_secs(120);
        DaemonData {
            daemon,
            stakes: Cached::new(Duration::from_secs(10), grace),
            fees: Cached::new(Duration::from_secs(30), grace),
            fork: Cached::new(Duration::from_secs(10), grace),
            distribution: Cached::new(Duration::from_secs(30), grace),
            tokens: moka::future::Cache::builder().max_capacity(10_000).time_to_live(Duration::from_secs(15)).build(),
        }
    }

    pub async fn stakes(&self) -> ApiResult<Arc<Stakes>> {
        self.stakes
            .get(|| async {
                let nodes = self.daemon.json_rpc("get_master_nodes", json!({})).await?;
                let blacklist = self.daemon.json_rpc("get_master_node_blacklisted_key_images", json!({})).await?;
                let mut out = Stakes::default();
                for item in arr(&blacklist, "blacklist") {
                    if let (Some(image), Some(amount)) = (
                        item.get("key_image").and_then(Json::as_str).and_then(hex32),
                        item.get("amount").and_then(Json::as_u64),
                    ) {
                        out.blacklisted.insert(image, amount);
                    }
                }
                for node in arr(&nodes, "master_node_states") {
                    for contributor in arr(node, "contributors") {
                        for c in arr(contributor, "locked_contributions") {
                            if let (Some(image), Some(amount)) = (
                                c.get("key_image").and_then(Json::as_str).and_then(hex32),
                                c.get("amount").and_then(Json::as_u64),
                            ) {
                                out.locked.insert(image, amount);
                            }
                        }
                    }
                }
                Ok(out)
            })
            .await
    }

    pub async fn fees(&self) -> ApiResult<Arc<Fees>> {
        self.fees
            .get(|| async {
                let r = self.daemon.json_rpc("get_fee_estimate", json!({"grace_blocks": 10})).await?;
                if r.get("status").and_then(Json::as_str) == Some("Failed") {
                    return Err(ApiError::Unavailable("get_fee_estimate failed".into()));
                }
                Ok(Fees {
                    fee_per_byte: u64_of(&r, "fee_per_byte")?,
                    fee_per_output: u64_of(&r, "fee_per_output")?,
                    flash_fee_per_byte: u64_of(&r, "flash_fee_per_byte")?,
                    flash_fee_per_output: u64_of(&r, "flash_fee_per_output")?,
                    flash_fee_fixed: u64_of(&r, "flash_fee_fixed")?,
                    quantization_mask: u64_of(&r, "quantization_mask")?,
                })
            })
            .await
    }

    pub async fn fork_version(&self) -> ApiResult<u64> {
        let v = self
            .fork
            .get(|| async {
                let r = self.daemon.json_rpc("hard_fork_info", json!({})).await?;
                match u64_of(&r, "version")? {
                    0 => Err(ApiError::Unavailable("hard_fork_info reported version 0".into())),
                    v => Ok(v),
                }
            })
            .await?;
        Ok(*v)
    }

    pub async fn distribution(&self) -> ApiResult<Arc<Distribution>> {
        self.distribution
            .get(|| async {
                let r = self
                    .daemon
                    .json_rpc(
                        "get_output_distribution",
                        json!({"amounts": [0], "from_height": 0, "to_height": 0, "cumulative": true}),
                    )
                    .await?;
                let dists: Vec<&Json> = arr(&r, "distributions").collect();
                let numbers = |d: &Json, key: &str| -> Vec<u64> { arr(d, key).filter_map(Json::as_u64).collect() };
                let mut out = Distribution::default();
                let mut native_seen = false;
                for d in &dists {
                    if d.get("amount").and_then(Json::as_u64) != Some(0) {
                        continue;
                    }
                    match d.get("filter_type").and_then(Json::as_u64).unwrap_or(1) {
                        1 if !native_seen => {
                            out.native = numbers(d, "distribution");
                            native_seen = true;
                        }
                        2 if out.token.is_none() && d.get("output_indices").is_some() => {
                            out.token = Some((numbers(d, "distribution"), numbers(d, "output_indices")));
                        }
                        _ => {}
                    }
                }
                if !native_seen {
                    return Err(ApiError::Unavailable(
                        "get_output_distribution returned no ringct distribution".into(),
                    ));
                }
                Ok(out)
            })
            .await
    }

    /// Token descriptor, or `None` when the daemon has no such token.
    pub async fn token(&self, id: &str) -> ApiResult<Option<Arc<TokenInfo>>> {
        if let Some(hit) = self.tokens.get(id).await {
            return Ok(hit);
        }
        let found = match self.daemon.json_rpc("get_token_info", json!({"token_id": id})).await {
            Ok(r) => Some(Arc::new(TokenInfo {
                token_id: r.get("token_id").and_then(Json::as_str).unwrap_or(id).to_owned(),
                ticker: str_of(&r, "ticker"),
                full_name: str_of(&r, "full_name"),
                owner: str_of(&r, "owner"),
                meta_info: str_of(&r, "meta_info"),
                current_supply: r.get("current_supply").and_then(Json::as_u64).unwrap_or(0),
                total_max_supply: r.get("total_max_supply").and_then(Json::as_u64).unwrap_or(0),
                decimal_point: r.get("decimal_point").and_then(Json::as_u64).unwrap_or(0) as u32,
            })),
            Err(lws_daemon::Error::Rpc { .. }) => None,
            Err(e) => return Err(e.into()),
        };
        self.tokens.insert(id.to_owned(), found.clone()).await;
        Ok(found)
    }
}
