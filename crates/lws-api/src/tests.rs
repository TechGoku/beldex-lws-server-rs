//! End-to-end tests of both routers against a real SQLite store and a mock
//! beldexd.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::routing::post;
use lws_core::{output_flags, AccountStatus, BlockId, Output, OutputRef, Spend, TokenData, MAX_BLOCK_NUMBER};
use lws_store::{AccountProgress, ScanCommit, Sealer};
use serde_json::{json, Value};
use tower::ServiceExt;

use super::*;
use crate::admin::{admin_router, AdminState};

const NET: Network = Network::Testnet;

async fn mock_daemon() -> String {
    async fn rpc(axum::Json(req): axum::Json<Value>) -> axum::Json<Value> {
        let result = match req["method"].as_str().unwrap_or("") {
            "get_info" => json!({"height": 1001, "target_height": 1001, "nettype": "testnet",
                                 "outgoing_connections_count": 8, "incoming_connections_count": 2}),
            "get_master_nodes" => json!({"master_node_states": [{"contributors": [{"locked_contributions": [
                {"key_image": hex::encode([0xaa; 32]), "amount": 5_000}]}]}]}),
            "get_master_node_blacklisted_key_images" => json!({"blacklist": []}),
            "get_fee_estimate" => json!({"fee_per_byte": 215, "fee_per_output": 20000, "flash_fee_per_byte": 645,
                "flash_fee_per_output": 60000, "flash_fee_fixed": 1000000, "quantization_mask": 10000, "status": "OK"}),
            "hard_fork_info" => json!({"version": 22}),
            "get_output_distribution" => json!({"distributions": [
                {"amount": 0, "filter_type": 1, "distribution": (1..=3000u64).map(|b| b * 10).collect::<Vec<_>>()}]}),
            "get_outs" => {
                let n = req["params"]["output_indices"].as_array().map(Vec::len).unwrap_or(0);
                json!({"outs": (0..n).map(|i| json!({"key": hex::encode([i as u8; 32]), "mask": hex::encode([7u8; 32]), "unlocked": true})).collect::<Vec<_>>()})
            }
            "send_raw_transaction" => json!({"status": "OK"}),
            "get_token_info" => return axum::Json(json!({"error": {"code": -1, "message": "not found"}})),
            other => panic!("unexpected daemon call {other}"),
        };
        axum::Json(json!({"jsonrpc": "2.0", "id": "0", "result": result}))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, Router::new().route("/json_rpc", post(rpc))).await.unwrap() });
    format!("http://{addr}")
}

struct Harness {
    state: Arc<AppState>,
    public: Router,
    admin: Router,
}

async fn harness(config: Config) -> Harness {
    let path = std::env::temp_dir().join(format!(
        "lws-api-test-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let store = Arc::new(
        Store::connect(&format!("sqlite:{}", path.display()), 4, Sealer::new((1, &[3; 32]), &[])).await.unwrap(),
    );
    store.migrate().await.unwrap();
    let daemon = Arc::new(Daemon::new(&[mock_daemon().await], Duration::from_secs(5), Duration::from_secs(5)).unwrap());
    let metrics = Arc::new(Metrics::default());
    Metrics::set(&metrics.daemon_height, 1001);
    let state = AppState::new(Config { network: NET, ..config }, store.clone(), store, daemon, metrics);
    let admin = admin_router(Arc::new(AdminState {
        app: state.clone(),
        token: Some(b"s3cret-token".to_vec()),
        info: json!({"instance": "test"}),
    }));
    Harness { public: public_router(state.clone()), admin, state }
}

async fn call(router: &Router, path: &str, body: Value, auth: Option<&str>) -> (StatusCode, Value) {
    let mut req = HttpRequest::post(path).header("content-type", "application/json");
    if let Some(token) = auth {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = router.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

struct Wallet {
    address: String,
    view_key: String,
}

fn wallet() -> Wallet {
    let (view_sec, view_pub) = lws_kernel::generate_keypair();
    let (_, spend_pub) = lws_kernel::generate_keypair();
    Wallet { address: lws_kernel::encode_address(&view_pub, &spend_pub, NET), view_key: hex::encode(view_sec) }
}

fn creds(w: &Wallet) -> Value {
    json!({"address": w.address, "view_key": w.view_key})
}

fn with(mut base: Value, extra: Value) -> Value {
    base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    base
}

fn output(account: i64, height: u64, global_index: u64, amount: u64) -> Output {
    Output {
        account_id: account,
        height,
        tx_hash: [height as u8; 32],
        tx_prefix_hash: [1; 32],
        tx_pub_key: [2; 32],
        out_index: 0,
        source: OutputRef { amount: 0, global_index },
        amount,
        mask: [3; 32],
        pub_key: [global_index as u8; 32],
        unlock_time: 0,
        timestamp: 1_700_000_000 + height,
        mixin: 15,
        flags: output_flags::RINGCT,
        payment_id: None,
        locked_key_image: None,
        token: None,
        rct: Some(vec![9; 96]),
    }
}

/// Logs the wallet in and stores a small history for it:
/// height 900: +1000 (gi 1)    height 950: +2000 (gi 2, stake 0xaa)
/// height 960: spends gi 1     height 970: +3000 locked until 2000
/// height 980: +50 of a token (gi 4)
async fn seeded(h: &Harness, w: &Wallet) -> i64 {
    let (status, body) =
        call(&h.public, "/login", with(creds(w), json!({"create_account": true, "generated_locally": true})), None)
            .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let account = h.state.primary.account_by_address(&w.address).await.unwrap().unwrap();
    h.state.primary.rescan(std::slice::from_ref(&w.address), 800).await.unwrap();

    let mut staked = output(account.id, 950, 2, 2000);
    staked.locked_key_image = Some([0xaa; 32]);
    let mut locked = output(account.id, 970, 3, 3000);
    locked.unlock_time = 2000;
    let mut token = output(account.id, 980, 4, 50);
    token.flags |= output_flags::TOKEN;
    token.token = Some(TokenData {
        token_id: [0x77; 32],
        blinded_token_id: [1; 32],
        amount_commitment: [2; 32],
        encrypted_amount: 99,
    });
    let spend = Spend {
        account_id: account.id,
        height: 960,
        tx_hash: [0xee; 32],
        key_image: [0x55; 32],
        source: OutputRef { amount: 0, global_index: 1 },
        timestamp: 1_700_000_960,
        unlock_time: 0,
        mixin: 15,
        payment_id: None,
    };
    let epoch = h.state.primary.chain_epoch().await.unwrap();
    h.state
        .primary
        .commit_scan(&ScanCommit {
            epoch,
            blocks: vec![BlockId { height: 1000, hash: [1; 32] }],
            accounts: vec![AccountProgress { id: account.id, generation: 1, from_height: 800, to_height: 1000 }],
            outputs: vec![output(account.id, 900, 1, 1000), staked, locked, token],
            spends: vec![spend],
        })
        .await
        .unwrap();
    account.id
}

#[tokio::test]
async fn login_and_authentication() {
    let h = harness(Config::default()).await;
    let w = wallet();
    let (s, b) = call(&h.public, "/login", with(creds(&w), json!({"create_account": false})), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) =
        call(&h.public, "/login", with(creds(&w), json!({"create_account": true, "generated_locally": true})), None)
            .await;
    assert_eq!((s, b["new_address"].as_bool(), b["start_height"].as_u64()), (StatusCode::OK, Some(true), Some(1000)));
    let (_, b) = call(&h.public, "/login", with(creds(&w), json!({"create_account": true})), None).await;
    assert_eq!((b["new_address"].as_bool(), b["generated_locally"].as_bool()), (Some(false), Some(true)));

    let other = wallet();
    let wrong_key = json!({"address": w.address, "view_key": other.view_key});
    assert_eq!(call(&h.public, "/get_address_info", wrong_key, None).await.0, StatusCode::FORBIDDEN);
    let bad_addr = json!({"address": "nope", "view_key": w.view_key});
    assert_eq!(call(&h.public, "/get_address_info", bad_addr, None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&h.public, "/get_address_info", json!({"bogus": 1}), None).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&h.public, "/nope", json!({}), None).await.0, StatusCode::NOT_FOUND);

    h.state.primary.set_status(std::slice::from_ref(&w.address), AccountStatus::Hidden).await.unwrap();
    assert_eq!(call(&h.public, "/get_address_info", creds(&w), None).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn creation_modes() {
    let h = harness(Config { creation: Creation::Disabled, ..Config::default() }).await;
    let (s, _) = call(&h.public, "/login", with(creds(&wallet()), json!({"create_account": true})), None).await;
    assert_eq!(s, StatusCode::NOT_IMPLEMENTED);

    let h = harness(Config { creation: Creation::Manual, ..Config::default() }).await;
    let w = wallet();
    let (s, _) = call(&h.public, "/login", with(creds(&w), json!({"create_account": true})), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(call(&h.public, "/get_address_info", creds(&w), None).await.0, StatusCode::FORBIDDEN);
    let (s, b) = call(
        &h.admin,
        "/accept_requests",
        json!({"params": {"type": "create", "addresses": [w.address]}}),
        Some("s3cret-token"),
    )
    .await;
    assert_eq!((s, b["updated"].as_array().map(Vec::len)), (StatusCode::OK, Some(1)));
    assert_eq!(call(&h.public, "/get_address_info", creds(&w), None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn balances_and_history() {
    let h = harness(Config::default()).await;
    let w = wallet();
    seeded(&h, &w).await;

    let (s, info) = call(&h.public, "/get_address_info", creds(&w), None).await;
    assert_eq!(s, StatusCode::OK, "{info}");
    assert_eq!(info["total_received"], "6000", "token amounts stay out of BDX totals");
    assert_eq!(info["total_sent"], "1000");
    assert_eq!(info["locked_funds"], "8000", "3000 time-locked + 5000 staked");
    assert_eq!(info["scanned_height"], 1000);
    assert_eq!(info["blockchain_height"], 1000);
    assert_eq!(info["start_height"], 800);
    let spent = &info["spent_outputs"][0];
    assert_eq!((spent["amount"].as_str(), spent["out_index"].as_u64()), (Some("1000"), Some(0)));
    assert!(spent.get("height").is_none(), "height only for incremental callers");
    assert_eq!(info["tokens"][0]["token_id"], hex::encode([0x77; 32]));
    assert_eq!(info["tokens"][0]["total_received"], "50");

    let (_, inc) = call(&h.public, "/get_address_info", with(creds(&w), json!({"min_height": 961})), None).await;
    assert!(inc["spent_outputs"].as_array().unwrap().is_empty());
    assert_eq!(inc["total_sent"], "0");

    let (s, txs) = call(&h.public, "/get_address_txs", creds(&w), None).await;
    assert_eq!(s, StatusCode::OK, "{txs}");
    let list = txs["transactions"].as_array().unwrap();
    assert_eq!(list.len(), 5);
    assert_eq!(list.iter().map(|t| t["height"].as_u64().unwrap()).collect::<Vec<_>>(), vec![900, 950, 960, 970, 980]);
    assert_eq!(list[2]["total_sent"], "1000");
    assert_eq!(list[2]["spent_outputs"][0]["key_image"], hex::encode([0x55; 32]));
    assert_eq!(list[4]["total_received"], "0");
    assert_eq!(list[4]["token_legs"][0]["received"], "50");
    assert_eq!(list[0]["timestamp"], "2023-11-14T22:28:20Z");
    assert!(list[0].get("token_legs").is_none());

    let (_, page) = call(&h.public, "/get_address_txs", with(creds(&w), json!({"max_count": 2})), None).await;
    assert_eq!(page["transactions"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_min_height"], 960);
    let (_, rest) = call(&h.public, "/get_address_txs", with(creds(&w), json!({"min_height": 960})), None).await;
    assert_eq!(rest["transactions"].as_array().unwrap().len(), 3);
    assert_eq!(rest["transactions"][0]["spent_outputs"][0]["height"], 960);
}

#[tokio::test]
async fn unspent_outputs() {
    let h = harness(Config::default()).await;
    let w = wallet();
    seeded(&h, &w).await;

    let (s, u) = call(&h.public, "/get_unspent_outs", with(creds(&w), json!({"amount": "0", "mixin": 10})), None).await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(u["fee_per_byte"], 215);
    assert_eq!(u["fork_version"], 22);
    // gi 1 (spent candidate, still listed with its key image) and gi 2 (its
    // stake image matches, but a stake only excludes an output of exactly the
    // staked amount); gi 3 is time-locked and gi 4 is a token.
    let outs = u["outputs"].as_array().unwrap();
    assert_eq!(outs.len(), 2, "{u}");
    assert_eq!(outs[0]["global_index"], 1);
    assert_eq!(outs[0]["tx_id"], 1);
    assert_eq!(outs[0]["spend_key_images"][0], hex::encode([0x55; 32]));
    assert_eq!(outs[0]["rct"], hex::encode([9; 96]));
    assert_eq!(u["amount"], "3000");

    let (_, t) =
        call(&h.public, "/get_unspent_outs", with(creds(&w), json!({"amount": "0", "all_tokens": true})), None).await;
    let token = t["outputs"].as_array().unwrap().iter().find(|o| o.get("token_id").is_some()).unwrap();
    assert_eq!((token["encrypted_amount"].as_str(), token["amount"].as_str()), (Some("99"), Some("50")));
    assert_eq!(t["amount"], "3000", "tokens never count toward the BDX amount");

    let (s, _) = call(&h.public, "/get_unspent_outs", with(creds(&w), json!({"amount": "999999"})), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn decoys_and_submission() {
    let h = harness(Config::default()).await;
    let (s, r) = call(&h.public, "/get_random_outs", json!({"count": 16, "amounts": ["0", "0"]}), None).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let rings = r["amount_outs"].as_array().unwrap();
    assert_eq!(rings.len(), 2);
    for ring in rings {
        let outs = ring["outputs"].as_array().unwrap();
        assert_eq!(outs.len(), 16);
        let mut seen: Vec<&str> = outs.iter().map(|o| o["global_index"].as_str().unwrap()).collect();
        seen.dedup();
        assert_eq!(seen.len(), 16, "decoys are distinct");
        assert_eq!(outs[0]["rct"].as_str().unwrap().len(), 192);
    }
    assert_eq!(
        call(&h.public, "/get_random_outs", json!({"count": 51, "amounts": ["0"]}), None).await.0,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(call(&h.public, "/submit_raw_tx", json!({"tx": "zz"}), None).await.0, StatusCode::BAD_REQUEST);
    let (s, b) = call(&h.public, "/submit_raw_tx", json!({"tx": "0102", "fee": "5"}), None).await;
    assert_eq!((s, b["status"].as_str()), (StatusCode::OK, Some("OK")));

    let (s, d) = call(&h.public, "/daemon_status", json!({}), None).await;
    assert_eq!((s, d["state"].as_str(), d["network"].as_str()), (StatusCode::OK, Some("ok"), Some("test")));
    let (s, _) = call(&h.public, "/get_token_info", json!({"token_id": hex::encode([1; 32])}), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "unknown token");
}

#[tokio::test]
async fn rate_limits_and_admin_auth() {
    let h = harness(Config { rate_limit: 0.001, rate_burst: 3.0, ..Config::default() }).await;
    for _ in 0..3 {
        assert_eq!(call(&h.public, "/daemon_status", json!({}), None).await.0, StatusCode::OK);
    }
    assert_eq!(call(&h.public, "/daemon_status", json!({}), None).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert!(Metrics::get(&h.state.metrics.http_rate_limited_total) >= 1);

    assert_eq!(call(&h.admin, "/list_accounts", json!({}), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&h.admin, "/list_accounts", json!({}), Some("wrong")).await.0, StatusCode::UNAUTHORIZED);
    let (s, b) = call(&h.admin, "/list_accounts", json!({}), Some("s3cret-token")).await;
    assert_eq!((s, b["active"].as_array().map(Vec::len)), (StatusCode::OK, Some(0)));

    let resp = h.admin.clone().oneshot(HttpRequest::get("/metrics").body(Body::empty()).unwrap()).await.unwrap();
    let text = String::from_utf8(axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    assert!(text.contains("beldex_lws_http_rate_limited_total"));
}

#[test]
fn timestamp_unlocks_use_wall_clock() {
    let stakes = crate::daemon_data::Stakes::default();
    let mut o = output(1, 10, 1, 7);
    o.unlock_time = MAX_BLOCK_NUMBER + 1; // 1985: long unlocked
    assert_eq!(wallet::tests_locked(&[o], &stakes, 10), 0);
}

/// The LMDB server's admin calls, unchanged, work against this server.
#[tokio::test]
async fn admin_api_is_compatible_with_the_lmdb_server() {
    let h = harness(Config::default()).await;
    let admin = wallet();
    let (view_public, spend_public) = lws_kernel::decode_address(&admin.address, NET).unwrap();
    h.state
        .primary
        .create_account(&lws_store::NewAccount {
            address: admin.address.clone(),
            view_public,
            spend_public,
            view_key: lws_core::ViewKey::from_hex(&admin.view_key).unwrap(),
            status: AccountStatus::Hidden,
            flags: lws_core::account_flags::ADMIN,
            start_height: 0,
        })
        .await
        .unwrap();
    let auth = |params: Value| json!({"auth": admin.view_key, "params": params});

    let (s, b) = call(&h.admin, "/list_accounts", auth(json!({})), None).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["hidden"][0]["address"], admin.address);
    assert!(b["hidden"][0].get("scan_height").is_some() && b["hidden"][0].get("access_time").is_some());
    let stranger = wallet();
    assert_eq!(
        call(&h.admin, "/list_accounts", json!({"auth": stranger.view_key}), None).await.0,
        StatusCode::FORBIDDEN
    );

    let user = wallet();
    let (s, b) =
        call(&h.admin, "/add_account", auth(json!({"address": user.address, "key": user.view_key})), None).await;
    assert_eq!((s, b["updated"][0].as_str()), (StatusCode::OK, Some(user.address.as_str())));
    let (_, b) =
        call(&h.admin, "/rescan", auth(json!({"height": 5, "addresses": [user.address, "unknown"]})), None).await;
    assert_eq!(b["updated"], json!([user.address]));
    let (_, b) = call(
        &h.admin,
        "/modify_account_status",
        auth(json!({"status": "inactive", "addresses": [user.address]})),
        None,
    )
    .await;
    assert_eq!(b["updated"], json!([user.address]));
    let (_, b) =
        call(&h.admin, "/reject_requests", auth(json!({"type": "import", "addresses": [user.address]})), None).await;
    assert_eq!(b["updated"], json!([]));
    let (_, b) = call(&h.admin, "/list_requests", auth(json!({})), None).await;
    assert!(b.get("create").is_some() && b.get("import").is_some());

    let (vp, sp) = lws_kernel::decode_address(&user.address, NET).unwrap();
    let ok =
        json!({"spend_public_hex": hex::encode(sp), "view_public_hex": hex::encode(vp), "view_key_hex": user.view_key});
    let (_, b) = call(&h.admin, "/validate", auth(ok.clone()), None).await;
    assert_eq!(b["address"], user.address);
    let mut wrong = ok.clone();
    wrong["view_key_hex"] = json!(stranger.view_key);
    let (_, b) = call(&h.admin, "/validate", auth(wrong), None).await;
    assert_eq!(b["error"]["field"], "view_key_hex");
    let mut short = ok;
    short["spend_public_hex"] = json!("abcd");
    let (_, b) = call(&h.admin, "/validate", auth(short), None).await;
    assert_eq!(b["error"]["details"], "Expected 64 characters");

    assert_eq!(call(&h.admin, "/switch_db", auth(json!({"path": "/x"})), None).await.0, StatusCode::NOT_IMPLEMENTED);

    // Changes made the LMDB way are audited too, attributed to the admin account.
    let (_, log) = get_json(&h.admin, "/api/v1/audit").await;
    let entries = log["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["action"] == "rescan"));
    assert!(entries.iter().all(|e| e["actor"].as_str().unwrap().starts_with("admin-account ")), "{log}");
}

async fn get(router: &Router, path: &str, auth: Option<&str>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = HttpRequest::get(path);
    if let Some(token) = auth {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let resp = router.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let (status, headers) = (resp.status(), resp.headers().clone());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, headers, bytes.to_vec())
}

async fn get_json(router: &Router, path: &str) -> (StatusCode, Value) {
    let (s, _, b) = get(router, path, Some("s3cret-token")).await;
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

const TOKEN: Option<&str> = Some("s3cret-token");

#[tokio::test]
async fn dashboard_is_served_under_a_strict_policy() {
    let h = harness(Config::default()).await;
    let (s, headers, body) = get(&h.admin, "/", None).await;
    assert_eq!(s, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(html.contains("Beldex LWS") && !html.contains("{{VERSION}}"));
    assert!(!html.contains("style=\""), "inline styles would break under the CSP");
    let csp = headers["content-security-policy"].to_str().unwrap();
    assert!(
        csp.contains("default-src 'none'")
            && csp.contains("script-src 'self'")
            && csp.contains("frame-ancestors 'none'")
    );
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    let (s, headers, _) = get(&h.admin, "/ui/app.js", None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(headers["content-type"].to_str().unwrap().starts_with("text/javascript"));
    // API answers are never cached.
    let (_, headers, _) = get(&h.admin, "/api/v1/overview", TOKEN).await;
    assert_eq!(headers["cache-control"], "no-store");
}

#[tokio::test]
async fn the_admin_json_api_needs_the_token() {
    let h = harness(Config::default()).await;
    for path in ["/api/v1/overview", "/api/v1/accounts", "/api/v1/audit", "/api/v1/config", "/api/v1/requests"] {
        assert_eq!(get(&h.admin, path, None).await.0, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(get(&h.admin, path, Some("wrong-token!")).await.0, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(get(&h.admin, path, TOKEN).await.0, StatusCode::OK, "{path}");
    }
    let (_, o) = get_json(&h.admin, "/api/v1/overview").await;
    assert_eq!(o["status"], "ok");
    assert_eq!(o["network"], "testnet");
    assert_eq!(o["chain"]["height"], 1001);
    assert_eq!(o["accounts"]["total"], 0);
    assert!(o["metrics"]["http_request_duration_seconds"]["bounds"].is_array());
    assert_eq!(o["server"]["instance"], "test");
    let (_, c) = get_json(&h.admin, "/api/v1/config").await;
    assert!(!c.to_string().contains("s3cret"), "no secrets in the config view");
}

#[tokio::test]
async fn accounts_are_managed_and_audited_through_the_json_api() {
    let h = harness(Config::default()).await;
    let wallets: Vec<Wallet> = (0..3).map(|_| wallet()).collect();
    for (i, w) in wallets.iter().enumerate() {
        let body = json!({"address": w.address, "view_key": w.view_key, "start_height": i * 10});
        let (s, b) = call(&h.admin, "/api/v1/accounts", body, TOKEN).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["created"], true);
    }
    let (s, b) = call(
        &h.admin,
        "/api/v1/accounts",
        json!({"address": wallets[0].address, "view_key": wallets[0].view_key}),
        TOKEN,
    )
    .await;
    assert_eq!((s, &b["created"]), (StatusCode::OK, &json!(false)));
    let (s, b) = call(
        &h.admin,
        "/api/v1/accounts",
        json!({"address": wallets[0].address, "view_key": wallets[1].view_key}),
        TOKEN,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("does not belong"), "{b}");

    // Keyset pages, newest first.
    let (_, page) = get_json(&h.admin, "/api/v1/accounts?limit=2").await;
    assert_eq!(page["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(page["accounts"][0]["address"], wallets[2].address);
    let next = page["next_before"].as_i64().unwrap();
    let (_, rest) = get_json(&h.admin, &format!("/api/v1/accounts?limit=2&before={next}")).await;
    assert_eq!(rest["accounts"].as_array().unwrap().len(), 1);
    assert!(rest["next_before"].is_null());
    assert!(page["accounts"][0].get("view_key").is_none(), "view keys are never returned");

    let prefix = &wallets[1].address[..20];
    let (_, found) = get_json(&h.admin, &format!("/api/v1/accounts?q={prefix}")).await;
    assert_eq!(found["accounts"][0]["address"], wallets[1].address);
    let (_, behind) = get_json(&h.admin, "/api/v1/accounts?behind=true").await;
    assert_eq!(behind["accounts"].as_array().unwrap().len(), 3);

    let (_, detail) = get_json(&h.admin, &format!("/api/v1/accounts/{}", wallets[1].address)).await;
    assert_eq!((detail["start_height"].as_u64(), detail["outputs"].as_u64()), (Some(10), Some(0)));
    assert_eq!(detail["behind"], 1000 - 10);

    let (s, b) = call(
        &h.admin,
        "/api/v1/accounts/status",
        json!({"status": "inactive", "addresses": [wallets[0].address]}),
        TOKEN,
    )
    .await;
    assert_eq!((s, b["updated"].as_array().unwrap().len()), (StatusCode::OK, 1));
    let (_, inactive) = get_json(&h.admin, "/api/v1/accounts?status=inactive").await;
    assert_eq!(inactive["accounts"][0]["address"], wallets[0].address);
    let (s, b) = call(
        &h.admin,
        "/api/v1/accounts/status",
        json!({"status": "deleted", "addresses": [wallets[0].address]}),
        TOKEN,
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, _) = call(&h.admin, "/api/v1/accounts/rescan", json!({"height": 3, "addresses": []}), TOKEN).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "an empty list is refused");
    let (_, b) =
        call(&h.admin, "/api/v1/accounts/rescan", json!({"height": 3, "addresses": [wallets[2].address]}), TOKEN).await;
    assert_eq!(b["updated"], json!([wallets[2].address]));

    let (_, o) = get_json(&h.admin, "/api/v1/overview").await;
    assert_eq!((o["accounts"]["active"].as_u64(), o["accounts"]["inactive"].as_u64()), (Some(2), Some(1)));
    assert_eq!(o["metrics"]["admin_actions_total"], 5);

    let (_, log) = get_json(&h.admin, "/api/v1/audit").await;
    let actions: Vec<&str> = log["entries"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, ["rescan", "set_status", "add_account", "add_account", "add_account"]);
    assert_eq!(log["entries"][0]["actor"], "token");
    assert_eq!(log["entries"][0]["detail"]["height"], 3);
    assert!(!log.to_string().contains(&wallets[0].view_key), "view keys never reach the audit log");
}

#[tokio::test]
async fn requests_are_decided_through_the_json_api() {
    let h = harness(Config { creation: Creation::Manual, ..Config::default() }).await;
    let w = wallet();
    let (s, _) = call(
        &h.public,
        "/login",
        json!({"address": w.address, "view_key": w.view_key, "create_account": true, "generated_locally": true}),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, r) = get_json(&h.admin, "/api/v1/requests").await;
    assert_eq!(r["requests"][0]["kind"], "create");
    let (_, o) = get_json(&h.admin, "/api/v1/overview").await;
    assert_eq!(o["requests"]["create"], 1);
    let (s, b) =
        call(&h.admin, "/api/v1/requests/accept", json!({"kind": "create", "addresses": [w.address]}), TOKEN).await;
    assert_eq!((s, b["updated"].as_array().unwrap().len()), (StatusCode::OK, 1));
    let (_, r) = get_json(&h.admin, "/api/v1/requests").await;
    assert!(r["requests"].as_array().unwrap().is_empty());
    let (s, _) =
        call(&h.admin, "/api/v1/requests/reject", json!({"kind": "other", "addresses": [w.address]}), TOKEN).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
