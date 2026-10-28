use super::*;
use axum::routing::post;
use axum::Router;

async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn blocks_response() -> Vec<u8> {
    let mut tx_indices = epee::Section::default();
    tx_indices.insert("indices", epee::Value::Array(vec![epee::Value::Uint(5), epee::Value::Uint(6)]));
    let mut block_indices = epee::Section::default();
    block_indices.insert("indices", epee::Value::Array(vec![epee::Value::Object(tx_indices)]));
    let mut entry = epee::Section::default();
    entry.insert("block", epee::Value::Bytes(Bytes::from_static(b"block-blob")));
    let mut root = epee::Section::default();
    root.insert("blocks", epee::Value::Array(vec![epee::Value::Object(entry)]));
    root.insert("start_height", epee::Value::Uint(41));
    root.insert("current_height", epee::Value::Uint(100));
    root.insert("status", epee::Value::Bytes(Bytes::from_static(b"OK")));
    root.insert("output_indices", epee::Value::Array(vec![epee::Value::Object(block_indices)]));
    root.insert("untrusted", epee::Value::Bool(false));
    epee::encode(&root)
}

#[test]
fn urls_normalise() {
    assert_eq!(normalize_url("127.0.0.1:19091"), "http://127.0.0.1:19091");
    assert_eq!(normalize_url("http://h:1/json_rpc"), "http://h:1");
    assert_eq!(normalize_url("https://h:1/ "), "https://h:1");
}

#[test]
fn singleton_wrappers_are_removed() {
    assert_eq!(unwrap_singletons(json!([[{"a": 1}]])), json!({"a": 1}));
    assert_eq!(unwrap_singletons(json!([1, 2])), json!([1, 2]));
}

#[tokio::test]
async fn fails_over_and_benches_dead_endpoints() {
    let good = serve(Router::new().route(
        "/json_rpc",
        post(|| async {
            axum::Json(json!([{"jsonrpc": "2.0", "id": "0", "result": [{"height": 77, "nettype": "testnet"}]}]))
        }),
    ))
    .await;
    let dead = "http://127.0.0.1:1".to_owned();
    let daemon = Daemon::new(&[dead.clone(), good.clone()], Duration::from_secs(2), Duration::from_secs(2)).unwrap();

    let info = daemon.get_info().await.unwrap();
    assert_eq!((info.height, info.nettype.as_str()), (77, "testnet"));
    let status = daemon.status();
    assert!(!status[0].healthy && status[0].consecutive_failures == 1);
    assert!(status[1].healthy);
    // The benched endpoint is now tried last.
    assert_eq!(daemon.order(0), vec![1, 0]);
}

#[tokio::test]
async fn rpc_errors_are_reported_not_retried() {
    let url =
        serve(Router::new().route(
            "/json_rpc",
            post(|| async { axum::Json(json!({"error": {"code": -2, "message": "no such token"}})) }),
        ))
        .await;
    let daemon = Daemon::new(&[url], Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    match daemon.json_rpc("get_token_info", json!({})).await {
        Err(Error::Rpc { code: -2, message }) => assert_eq!(message, "no such token"),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn get_blocks_round_trip() {
    let url = serve(Router::new().route(
        "/get_blocks.bin",
        post(|body: Bytes| async move {
            let req = epee::decode(body).unwrap();
            assert_eq!(req.u64("start_height").unwrap(), 0);
            assert_eq!(req.bytes("block_ids").unwrap().len(), 64);
            blocks_response()
        }),
    ))
    .await;
    let daemon = Daemon::new(&[url], Duration::from_secs(2), Duration::from_secs(2)).unwrap();
    let request = GetBlocks { start_height: 0, block_ids: vec![[1; 32], [0; 32]], prune: true };
    let blocks = daemon.get_blocks(&request, 0).await.unwrap();
    assert_eq!((blocks.start_height, blocks.current_height), (41, 100));
    assert_eq!(blocks.blocks.len(), 1);
    assert_eq!(&blocks.blocks[0].block[..], b"block-blob");
    assert!(blocks.blocks[0].txs.is_empty());
    assert_eq!(blocks.blocks[0].output_indices, vec![vec![5, 6]]);
}

#[test]
fn inconsistent_block_responses_are_rejected() {
    let mut root = epee::Section::default();
    root.insert("status", epee::Value::Bytes(Bytes::from_static(b"Failed")));
    assert!(matches!(parse_blocks(Bytes::from(epee::encode(&root))), Err(Error::Status(s)) if s == "Failed"));

    let mut root = epee::decode(Bytes::from(blocks_response())).unwrap();
    root.0.retain(|(k, _)| k != "output_indices");
    assert!(matches!(parse_blocks(Bytes::from(epee::encode(&root))), Err(Error::Decode(_))));
}
