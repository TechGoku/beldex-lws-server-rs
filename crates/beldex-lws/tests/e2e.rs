//! End to end: a mock beldexd serves a real consensus-format chain (built by
//! kernel/tests/make_chain.cpp) over `get_blocks.bin`; the real scanner scans
//! it into SQLite through the C++ kernel; the real API answers wallet calls.
//! Then the daemon switches to a competing fork and the server must roll the
//! abandoned blocks back.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use axum::routing::post;
use axum::Router;
use bytes::Bytes;
use lws_core::metrics::Metrics;
use lws_core::{AccountStatus, Network, ViewKey};
use lws_daemon::epee::{self, Section, Value as Ev};
use lws_daemon::Daemon;
use lws_store::{NewAccount, Sealer, Store};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Block {
    hash: [u8; 32],
    blob: Vec<u8>,
    txs: Vec<Vec<u8>>,
    indices: Vec<Vec<u64>>,
}

struct Fixture {
    address: String,
    view_key: String,
    key_image: String,
    reward: u64,
    change: u64,
    a: Vec<Block>,
    b: Vec<Block>,
}

fn load() -> Fixture {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/chain.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let chain = |name: &str| -> Vec<Block> {
        v["chains"][name]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| Block {
                hash: lws_core::parse_key(b["hash"].as_str().unwrap()).unwrap(),
                blob: hex::decode(b["block"].as_str().unwrap()).unwrap(),
                txs: b["txs"].as_array().unwrap().iter().map(|t| hex::decode(t.as_str().unwrap()).unwrap()).collect(),
                indices: b["output_indices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tx| tx.as_array().unwrap().iter().map(|i| i.as_u64().unwrap()).collect())
                    .collect(),
            })
            .collect()
    };
    Fixture {
        address: v["address"].as_str().unwrap().to_owned(),
        view_key: v["view_key"].as_str().unwrap().to_owned(),
        key_image: v["key_image"].as_str().unwrap().to_owned(),
        reward: v["reward"].as_u64().unwrap(),
        change: v["change"].as_u64().unwrap(),
        a: chain("a"),
        b: chain("b"),
    }
}

/// A beldexd stand-in serving one of the fixture's chains. `cap` limits the
/// blocks per response (as size limits do) and `behind` hides the chain's
/// last blocks, like a daemon that has not caught up.
struct MockDaemon {
    fixture: Arc<Fixture>,
    use_b: Arc<Mutex<bool>>,
    cap: usize,
    behind: usize,
    served: AtomicUsize,
}

impl MockDaemon {
    fn new(fixture: Arc<Fixture>, use_b: Arc<Mutex<bool>>, cap: usize, behind: usize) -> Arc<MockDaemon> {
        Arc::new(MockDaemon { fixture, use_b, cap, behind, served: AtomicUsize::new(0) })
    }

    fn chain(&self) -> &[Block] {
        let full = if *self.use_b.lock().unwrap() { &self.fixture.b } else { &self.fixture.a };
        &full[..full.len() - self.behind.min(full.len() - 1)]
    }

    fn get_blocks(&self, body: Bytes) -> Vec<u8> {
        let req = epee::decode(body).unwrap();
        let chain = self.chain();
        let ids = req.bytes("block_ids").map(|b| b.to_vec()).unwrap_or_default();
        let start = if ids.is_empty() {
            req.u64("start_height").unwrap() as usize
        } else {
            // Highest listed block that is on our chain, as the daemon does.
            ids.chunks(32)
                .find_map(|id| chain.iter().position(|b| b.hash[..] == *id))
                .expect("genesis is always shared")
        };
        let mut root = Section::default();
        if start >= chain.len() {
            root.insert("status", Ev::Bytes(Bytes::from_static(b"Failed")));
            return epee::encode(&root);
        }
        self.served.fetch_add(1, Ordering::Relaxed);
        let mut blocks = Vec::new();
        let mut indices = Vec::new();
        for b in chain[start..].iter().take(self.cap) {
            let mut entry = Section::default();
            entry.insert("block", Ev::Bytes(Bytes::from(b.blob.clone())));
            if !b.txs.is_empty() {
                entry.insert("txs", Ev::Array(b.txs.iter().map(|t| Ev::Bytes(Bytes::from(t.clone()))).collect()));
            }
            blocks.push(Ev::Object(entry));
            let per_tx = b
                .indices
                .iter()
                .map(|tx| {
                    let mut s = Section::default();
                    s.insert("indices", Ev::Array(tx.iter().map(|&i| Ev::Uint(i)).collect()));
                    Ev::Object(s)
                })
                .collect();
            let mut s = Section::default();
            s.insert("indices", Ev::Array(per_tx));
            indices.push(Ev::Object(s));
        }
        root.insert("blocks", Ev::Array(blocks));
        root.insert("start_height", Ev::Uint(start as u64));
        root.insert("current_height", Ev::Uint(chain.len() as u64));
        root.insert("status", Ev::Bytes(Bytes::from_static(b"OK")));
        root.insert("output_indices", Ev::Array(indices));
        epee::encode(&root)
    }

    fn rpc(&self, req: Value) -> Value {
        let chain = self.chain();
        let result = match req["method"].as_str().unwrap() {
            "get_info" => json!({"height": chain.len(), "target_height": chain.len(), "nettype": "testnet",
                                 "outgoing_connections_count": 1, "incoming_connections_count": 0}),
            "get_block_header_by_height" => {
                let h = req["params"]["height"].as_u64().unwrap() as usize;
                json!({"block_header": {"hash": hex::encode(chain[h].hash)}})
            }
            "get_master_nodes" => json!({"master_node_states": []}),
            "get_master_node_blacklisted_key_images" => json!({"blacklist": []}),
            "get_fee_estimate" => json!({"fee_per_byte": 1, "fee_per_output": 2, "flash_fee_per_byte": 3,
                "flash_fee_per_output": 4, "flash_fee_fixed": 5, "quantization_mask": 6}),
            "hard_fork_info" => json!({"version": 22}),
            other => panic!("unexpected daemon call {other}"),
        };
        json!({"jsonrpc": "2.0", "id": "0", "result": result})
    }
}

async fn start_daemon(mock: Arc<MockDaemon>) -> String {
    let (m1, m2) = (mock.clone(), mock);
    let app = Router::new()
        .route("/get_blocks.bin", post(move |body: Bytes| async move { m1.get_blocks(body) }))
        .route("/json_rpc", post(move |axum::Json(req): axum::Json<Value>| async move { axum::Json(m2.rpc(req)) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn call(router: &Router, path: &str, body: Value) -> Value {
    let req =
        Request::post(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(status.is_success(), "{path}: {status} {}", String::from_utf8_lossy(&bytes));
    serde_json::from_slice(&bytes).unwrap()
}

async fn scan_until(scanner: &lws_scanner::Scanner, store: &Store, id: i64, height: u64) {
    for _ in 0..50 {
        scanner.pass().await.unwrap();
        if store.account_by_id(id).await.unwrap().unwrap().scan_height == height {
            return;
        }
    }
    panic!("account never reached height {height}");
}

fn one_daemon() -> Vec<Arc<MockDaemon>> {
    vec![MockDaemon::new(Arc::new(load()), Arc::default(), usize::MAX, 0)]
}

/// Two daemons, one two blocks behind, both answering a few blocks at a time,
/// so the scanner's fetch window has to spread requests across them, fill
/// gaps, and get the last blocks from the one that has them.
fn two_daemons() -> Vec<Arc<MockDaemon>> {
    let (fixture, use_b) = (Arc::new(load()), Arc::new(Mutex::new(false)));
    vec![MockDaemon::new(fixture.clone(), use_b.clone(), 3, 2), MockDaemon::new(fixture, use_b, 2, 0)]
}

fn temp_sqlite(tag: &str) -> (String, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("lws-e2e-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    (format!("sqlite:{}", path.display()), path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_scans_serves_and_survives_a_reorg() {
    let (url, path) = temp_sqlite("one");
    scenario(&url, one_daemon(), 1).await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn several_daemons_fetch_in_parallel() {
    let (url, path) = temp_sqlite("multi");
    let daemons = two_daemons();
    scenario(&url, daemons.clone(), 4).await;
    let served: Vec<usize> = daemons.iter().map(|d| d.served.load(Ordering::Relaxed)).collect();
    assert!(served.iter().all(|&n| n > 0), "both daemons served blocks: {served:?}");
    let _ = std::fs::remove_file(&path);
}

/// Runs only when `LWS_TEST_POSTGRES_URL` names a server we may create databases on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_scans_serves_and_survives_a_reorg() {
    let Ok(url) = std::env::var("LWS_TEST_POSTGRES_URL") else { return };
    let name = format!("lws_e2e_{}", std::process::id());
    let admin = sqlx_pg_admin(&url).await;
    admin(&format!("DROP DATABASE IF EXISTS {name}")).await;
    admin(&format!("CREATE DATABASE {name}")).await;
    let (base, _) = url.rsplit_once('/').unwrap();
    scenario(&format!("{base}/{name}"), one_daemon(), 1).await;
}

async fn sqlx_pg_admin(url: &str) -> impl Fn(&str) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    let url = url.to_owned();
    move |sql: &str| {
        let (url, sql) = (url.clone(), sql.to_owned());
        Box::pin(async move {
            let store = Store::connect(&url, 1, Sealer::plaintext()).await.unwrap();
            store.execute_admin_sql(&sql).await.unwrap();
        })
    }
}

async fn scenario(db_url: &str, daemons: Vec<Arc<MockDaemon>>, fetch_window: usize) {
    lws_kernel::self_check().unwrap();
    let mut urls = Vec::new();
    for d in &daemons {
        urls.push(start_daemon(d.clone()).await);
    }
    let mock = daemons[0].clone();
    let f = &mock.fixture;

    let store = Arc::new(Store::connect(db_url, 4, Sealer::new((1, &[8; 32]), &[])).await.unwrap());
    store.migrate().await.unwrap();
    store.check_network(Network::Testnet).await.unwrap();

    let (view_public, spend_public) = lws_kernel::decode_address(&f.address, Network::Testnet).unwrap();
    let account = match store
        .create_account(&NewAccount {
            address: f.address.clone(),
            view_public,
            spend_public,
            view_key: ViewKey::from_hex(&f.view_key).unwrap(),
            status: AccountStatus::Active,
            flags: 0,
            start_height: 0,
        })
        .await
        .unwrap()
    {
        lws_store::CreateOutcome::Created(a) => a,
        _ => unreachable!(),
    };

    let daemon = Arc::new(Daemon::new(&urls, Duration::from_secs(5), Duration::from_secs(5)).unwrap());
    daemon.refresh_heights().await;
    let metrics = Arc::new(Metrics::default());
    let config =
        lws_scanner::Config { network: Network::Testnet, lane_window: 100, fetch_window, ..Default::default() };
    let scanner = lws_scanner::Scanner::new(store.clone(), daemon.clone(), metrics.clone(), config, 2).unwrap();
    scanner.ensure_genesis().await.unwrap();
    scan_until(&scanner, &store, account.id, 10).await;

    let api_config = lws_api::Config { network: Network::Testnet, ..Default::default() };
    let state = lws_api::AppState::new(api_config, store.clone(), store.clone(), daemon, metrics.clone());
    let api = lws_api::public_router(state);
    let creds = json!({"address": f.address, "view_key": f.view_key});

    // Chain a: two coinbase receives, then a transfer paying change back. The
    // transfer's ring holds both wallet outputs, so both are candidate spends.
    let info = call(&api, "/get_address_info", creds.clone()).await;
    assert_eq!(info["scanned_height"], 10);
    assert_eq!(info["total_received"], (2 * f.reward + f.change).to_string());
    assert_eq!(info["total_sent"], (2 * f.reward).to_string());
    assert_eq!(info["locked_funds"], (2 * f.reward).to_string(), "coinbase unlocks at height+30");
    let spends = info["spent_outputs"].as_array().unwrap();
    assert_eq!(spends.len(), 2);
    assert!(spends.iter().all(|s| s["key_image"] == f.key_image));

    let txs = call(&api, "/get_address_txs", creds.clone()).await;
    let list = txs["transactions"].as_array().unwrap();
    assert_eq!(list.iter().map(|t| t["height"].as_u64().unwrap()).collect::<Vec<_>>(), vec![3, 5, 8]);
    assert!(list[0]["coinbase"].as_bool().unwrap());
    assert_eq!(list[2]["total_received"], f.change.to_string());
    assert_eq!(list[2]["total_sent"], (2 * f.reward).to_string());

    let unspent =
        call(&api, "/get_unspent_outs", json!({"address": f.address, "view_key": f.view_key, "amount": "0"})).await;
    let outs = unspent["outputs"].as_array().unwrap();
    assert_eq!(outs.len(), 1, "coinbase outputs are still locked");
    assert_eq!(outs[0]["amount"], f.change.to_string());
    assert_eq!(outs[0]["global_index"], 9);
    assert_eq!(outs[0]["rct"].as_str().unwrap().len(), 192);

    // The daemon moves to chain b, which forks at height 7 and is one longer.
    *mock.use_b.lock().unwrap() = true;
    scan_until(&scanner, &store, account.id, 11).await;
    assert!(Metrics::get(&metrics.reorgs_total) >= 1);
    assert_eq!(store.block_hash(7).await.unwrap().unwrap(), mock.fixture.b[7].hash);

    let info = call(&api, "/get_address_info", creds.clone()).await;
    assert_eq!(info["scanned_height"], 11);
    assert_eq!(info["total_received"], (2 * f.reward).to_string(), "the abandoned transfer is gone");
    assert_eq!(info["total_sent"], "0");
    assert!(info["spent_outputs"].as_array().unwrap().is_empty());
    let txs = call(&api, "/get_address_txs", creds).await;
    assert_eq!(txs["transactions"].as_array().unwrap().len(), 2);
}

/// A block announcement wakes an idle scanner once, promptly, and it does not
/// spin afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn block_announcements_wake_the_scanner_once() {
    let (db, path) = temp_sqlite("notify");
    let mock = one_daemon().remove(0);
    let url = start_daemon(mock.clone()).await;
    let store = Arc::new(Store::connect(&db, 4, Sealer::new((1, &[8; 32]), &[])).await.unwrap());
    store.migrate().await.unwrap();
    store.check_network(Network::Testnet).await.unwrap();
    let daemon = Arc::new(Daemon::new(&[url], Duration::from_secs(5), Duration::from_secs(5)).unwrap());
    let metrics = Arc::new(Metrics::default());
    let config =
        lws_scanner::Config { network: Network::Testnet, poll_interval: Duration::from_secs(30), ..Default::default() };
    let (announce, blocks) = tokio::sync::watch::channel(0u64);
    let scanner =
        lws_scanner::Scanner::new(store, daemon, metrics.clone(), config, 1).unwrap().with_block_notifications(blocks);
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move { scanner.run(shutdown).await });

    let passes = || Metrics::get(&metrics.scan_passes_total);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idle = passes();
    assert!(idle >= 1, "first pass ran");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(passes(), idle, "no polling within the 30 s interval");

    announce.send_replace(11);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(passes(), idle + 1, "one announcement, one pass");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(passes(), idle + 1, "no spinning after the announcement");

    stop.send_replace(true);
    task.await.unwrap().unwrap();
    let _ = std::fs::remove_file(&path);
}
