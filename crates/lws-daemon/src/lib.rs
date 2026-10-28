//! beldexd client.
//!
//! Every call goes through an endpoint pool: endpoints are tried in configured
//! order, an endpoint that fails is benched with exponential backoff, and a
//! benched endpoint is still tried as a last resort so a pool where everything
//! is benched recovers on the first call that succeeds. One beldexd restart
//! therefore costs a retry, not an outage.

pub mod epee;

use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{json, Value as Json};
use tracing::{debug, warn};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no daemon endpoint answered: {0}")]
    Unavailable(String),
    #[error("daemon returned RPC error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("unexpected daemon response: {0}")]
    Decode(String),
    #[error("daemon reported status '{0}'")]
    Status(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<epee::Error> for Error {
    fn from(e: epee::Error) -> Self {
        Error::Decode(e.to_string())
    }
}

#[derive(Default)]
struct Health {
    failures: u32,
    benched_until: Option<Instant>,
    last_error: Option<String>,
}

struct Endpoint {
    base: String,
    health: Mutex<Health>,
}

/// Point-in-time view of one endpoint, for `/health` and metrics.
#[derive(Clone, Debug, serde::Serialize)]
pub struct EndpointStatus {
    pub url: String,
    pub healthy: bool,
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

pub struct Daemon {
    endpoints: Vec<Endpoint>,
    http: reqwest::Client,
    rpc_timeout: Duration,
    blocks_timeout: Duration,
}

/// `http://host:port`, `http://host:port/` and `http://host:port/json_rpc` all
/// name the same daemon.
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/json_rpc").unwrap_or(trimmed);
    if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    }
}

/// Strips the single-element array wrappers some beldexd builds put around a
/// JSON-RPC envelope or its result.
fn unwrap_singletons(mut value: Json) -> Json {
    while let Json::Array(items) = &mut value {
        if items.len() != 1 {
            break;
        }
        value = items.pop().unwrap_or(Json::Null);
    }
    value
}

#[derive(Clone, Debug)]
pub struct Info {
    /// Number of blocks; the top block is `height - 1`.
    pub height: u64,
    pub target_height: u64,
    pub nettype: String,
    pub outgoing_connections: u64,
    pub incoming_connections: u64,
    pub top_block_hash: String,
}

#[derive(Clone, Debug, Default)]
pub struct GetBlocks {
    /// Used only when `block_ids` is empty.
    pub start_height: u64,
    /// Highest first, ending with genesis. The daemon starts at the highest of
    /// these that is on its main chain.
    pub block_ids: Vec<[u8; 32]>,
    pub prune: bool,
}

#[derive(Clone, Debug)]
pub struct RawBlock {
    pub block: Bytes,
    pub txs: Vec<Bytes>,
    /// Per tx, miner tx first.
    pub output_indices: Vec<Vec<u64>>,
}

#[derive(Clone, Debug)]
pub struct Blocks {
    pub start_height: u64,
    /// Number of blocks on the daemon's chain.
    pub current_height: u64,
    pub blocks: Vec<RawBlock>,
    pub untrusted: bool,
}

impl Daemon {
    pub fn new(urls: &[String], rpc_timeout: Duration, blocks_timeout: Duration) -> Result<Daemon> {
        if urls.is_empty() {
            return Err(Error::Unavailable("no daemon endpoints configured".into()));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .pool_idle_timeout(Duration::from_secs(60))
            .user_agent(concat!("beldex-lws/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Unavailable(e.to_string()))?;
        let mut endpoints: Vec<Endpoint> = Vec::new();
        for url in urls {
            let base = normalize_url(url);
            if !endpoints.iter().any(|e| e.base == base) {
                endpoints.push(Endpoint { base, health: Mutex::default() });
            }
        }
        Ok(Daemon { endpoints, http, rpc_timeout, blocks_timeout })
    }

    pub fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    pub fn status(&self) -> Vec<EndpointStatus> {
        let now = Instant::now();
        self.endpoints
            .iter()
            .map(|e| {
                let h = e.health.lock().unwrap();
                EndpointStatus {
                    url: e.base.clone(),
                    healthy: h.benched_until.is_none_or(|t| t <= now),
                    consecutive_failures: h.failures,
                    last_error: h.last_error.clone(),
                }
            })
            .collect()
    }

    /// Endpoint indices to try: unbenched ones first, starting at `start`
    /// (so scanner lanes can spread load), then benched ones.
    fn order(&self, start: usize) -> Vec<usize> {
        let now = Instant::now();
        let n = self.endpoints.len();
        let rotated = (0..n).map(|i| (start + i) % n);
        let (healthy, benched): (Vec<usize>, Vec<usize>) =
            rotated.partition(|&i| self.endpoints[i].health.lock().unwrap().benched_until.is_none_or(|t| t <= now));
        healthy.into_iter().chain(benched).collect()
    }

    fn record(&self, index: usize, outcome: std::result::Result<(), String>) {
        let mut h = self.endpoints[index].health.lock().unwrap();
        match outcome {
            Ok(()) => *h = Health::default(),
            Err(e) => {
                h.failures = h.failures.saturating_add(1);
                let backoff = Duration::from_secs((1u64 << h.failures.min(6)).min(60));
                h.benched_until = Some(Instant::now() + backoff);
                warn!(endpoint = %self.endpoints[index].base, failures = h.failures, error = %e, "daemon endpoint failed");
                h.last_error = Some(e);
            }
        }
    }

    async fn post(
        &self,
        start: usize,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
        timeout: Duration,
    ) -> Result<Bytes> {
        let mut last = String::new();
        for index in self.order(start) {
            let url = format!("{}{}", self.endpoints[index].base, path);
            let began = Instant::now();
            let sent = self
                .http
                .post(&url)
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .timeout(timeout)
                .body(body.clone())
                .send()
                .await;
            let outcome = match sent {
                Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                    Ok(bytes) => Ok(bytes),
                    Err(e) => Err(format!("reading body: {e}")),
                },
                Ok(resp) => Err(format!("HTTP {}", resp.status())),
                Err(e) if e.is_timeout() => Err(format!("timed out after {:?}", timeout)),
                Err(e) => Err(e.to_string()),
            };
            match outcome {
                Ok(bytes) => {
                    self.record(index, Ok(()));
                    debug!(%url, elapsed_ms = began.elapsed().as_millis() as u64, bytes = bytes.len(), "daemon call");
                    return Ok(bytes);
                }
                Err(e) => {
                    last = format!("{url}: {e}");
                    self.record(index, Err(e));
                }
            }
        }
        Err(Error::Unavailable(last))
    }

    /// Calls a JSON-RPC method and returns its `result`.
    pub async fn json_rpc(&self, method: &str, params: Json) -> Result<Json> {
        let body = serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": "0", "method": method, "params": params}))
            .map_err(|e| Error::Decode(e.to_string()))?;
        let bytes = self.post(0, "/json_rpc", body, "application/json", self.rpc_timeout).await?;
        let envelope: Json = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Decode(format!("{method}: not JSON ({e}); is the daemon URL right?")))?;
        let mut envelope = unwrap_singletons(envelope);
        if let Some(err) = envelope.get("error").filter(|e| !e.is_null()) {
            return Err(Error::Rpc {
                code: err.get("code").and_then(Json::as_i64).unwrap_or(0),
                message: err.get("message").and_then(Json::as_str).unwrap_or("").to_owned(),
            });
        }
        match envelope.get_mut("result") {
            Some(result) => Ok(unwrap_singletons(result.take())),
            None => Err(Error::Decode(format!("{method}: response has no result"))),
        }
    }

    pub async fn get_info(&self) -> Result<Info> {
        let r = self.json_rpc("get_info", json!({})).await?;
        let u = |k: &str| r.get(k).and_then(Json::as_u64).unwrap_or(0);
        let height =
            r.get("height").and_then(Json::as_u64).ok_or_else(|| Error::Decode("get_info: no height".into()))?;
        Ok(Info {
            height,
            target_height: u("target_height"),
            nettype: r.get("nettype").and_then(Json::as_str).unwrap_or("").to_owned(),
            outgoing_connections: u("outgoing_connections_count"),
            incoming_connections: u("incoming_connections_count"),
            top_block_hash: r.get("top_block_hash").and_then(Json::as_str).unwrap_or("").to_owned(),
        })
    }

    pub async fn block_hash(&self, height: u64) -> Result<[u8; 32]> {
        let r = self.json_rpc("get_block_header_by_height", json!({"height": height})).await?;
        let text = r
            .pointer("/block_header/hash")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::Decode("get_block_header_by_height: no hash".into()))?;
        let mut out = [0u8; 32];
        hex_decode(text, &mut out).ok_or_else(|| Error::Decode(format!("bad block hash '{text}'")))?;
        Ok(out)
    }

    /// `get_blocks.bin`. `lane` picks which endpoint to try first.
    pub async fn get_blocks(&self, request: &GetBlocks, lane: usize) -> Result<Blocks> {
        let mut ids = Vec::with_capacity(request.block_ids.len() * 32);
        request.block_ids.iter().for_each(|id| ids.extend_from_slice(id));
        let mut root = epee::Section::default();
        root.insert("block_ids", epee::Value::Bytes(Bytes::from(ids)));
        root.insert(
            "start_height",
            epee::Value::Uint(if request.block_ids.is_empty() { request.start_height } else { 0 }),
        );
        root.insert("prune", epee::Value::Bool(request.prune));
        root.insert("no_miner_tx", epee::Value::Bool(false));
        let body = epee::encode(&root);

        let raw = self.post(lane, "/get_blocks.bin", body, "application/octet-stream", self.blocks_timeout).await?;
        parse_blocks(raw)
    }

    pub async fn send_raw_transaction(&self, tx_hex: &str, flash: bool) -> Result<Json> {
        self.json_rpc("send_raw_transaction", json!({"tx": tx_hex, "flash": flash})).await
    }
}

/// Decodes a `get_blocks.bin` response and checks it is internally consistent.
pub fn parse_blocks(raw: Bytes) -> Result<Blocks> {
    let root = epee::decode(raw)?;
    let status = root.bytes("status").map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
    if status != "OK" {
        return Err(Error::Status(if status.is_empty() { "missing".into() } else { status }));
    }
    let entries = root.array("blocks")?;
    let indices = root.array("output_indices")?;
    if entries.len() != indices.len() {
        return Err(Error::Decode(format!("{} blocks but {} output index lists", entries.len(), indices.len())));
    }
    let mut blocks = Vec::with_capacity(entries.len());
    for (entry, idx) in entries.iter().zip(indices) {
        let entry = entry.as_object().ok_or(epee::Error::Field("blocks"))?;
        let txs = entry
            .array("txs")?
            .iter()
            .map(|t| t.as_bytes().cloned().ok_or(epee::Error::Field("txs")))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let per_tx = idx
            .as_object()
            .ok_or(epee::Error::Field("output_indices"))?
            .array("indices")?
            .iter()
            .map(|tx| {
                tx.as_object()
                    .ok_or(epee::Error::Field("indices"))?
                    .array("indices")?
                    .iter()
                    .map(|v| v.as_u64().ok_or(epee::Error::Field("indices")))
                    .collect::<std::result::Result<Vec<u64>, _>>()
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        blocks.push(RawBlock { block: entry.bytes("block")?.clone(), txs, output_indices: per_tx });
    }
    Ok(Blocks {
        start_height: root.u64("start_height")?,
        current_height: root.u64("current_height")?,
        blocks,
        untrusted: matches!(root.get("untrusted"), Some(epee::Value::Bool(true))),
    })
}

pub fn hex_decode(text: &str, out: &mut [u8]) -> Option<()> {
    if text.len() != out.len() * 2 {
        return None;
    }
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(())
}

#[cfg(test)]
mod tests;
