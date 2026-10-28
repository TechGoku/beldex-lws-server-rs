//! Scan throughput on this machine. Ignored by default; run with
//! `cargo test --release -p beldex-lws --test bench -- --ignored --nocapture`.

use std::time::Instant;

use lws_kernel::{Batch, BlockEntry, ScanKey};
use rayon::prelude::*;
use serde_json::Value;

type RawBlock = (Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u64>>);

#[test]
#[ignore]
fn kernel_scan_throughput() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/chain.json")).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    let blocks: Vec<RawBlock> = v["chains"]["a"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                hex::decode(b["block"].as_str().unwrap()).unwrap(),
                b["txs"].as_array().unwrap().iter().map(|t| hex::decode(t.as_str().unwrap()).unwrap()).collect(),
                b["output_indices"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_array().unwrap().iter().map(|i| i.as_u64().unwrap()).collect())
                    .collect(),
            )
        })
        .collect();
    let entries: Vec<BlockEntry> = blocks
        .iter()
        .map(|(b, t, i)| BlockEntry {
            block: b,
            txs: t.iter().map(|x| x.as_slice()).collect(),
            output_indices: i.clone(),
        })
        .collect();
    let batch = Batch::parse(&entries, true).unwrap();
    let txs_with_outputs = batch.view().txs.iter().filter(|t| t.output_count > 0).count();

    let accounts = 50_000;
    let keys: Vec<ScanKey> = (0..accounts)
        .map(|_| {
            let (view_secret, _) = lws_kernel::generate_keypair();
            let (_, spend_public) = lws_kernel::generate_keypair();
            ScanKey { view_secret, spend_public, scan_height: 0 }
        })
        .collect();

    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    for &n in &[1, threads] {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(n).build().unwrap();
        let began = Instant::now();
        let flagged =
            pool.install(|| lws_kernel::fast::candidates(&batch.view(), &keys)).iter().filter(|f| **f).count();
        let secs = began.elapsed().as_secs_f64();
        println!(
            "{n:>2} thread(s), fast prefilter: {accounts} accounts x {txs_with_outputs} txs in {secs:.2}s = {:.0} account-tx checks/s ({flagged} flagged)",
            (accounts * txs_with_outputs) as f64 / secs
        );
    }
    {
        let &n = &threads;
        let pool = rayon::ThreadPoolBuilder::new().num_threads(n).build().unwrap();
        let began = Instant::now();
        let found: usize = pool.install(|| keys.par_chunks(256).map(|c| batch.scan(c).unwrap().len()).sum());
        let secs = began.elapsed().as_secs_f64();
        let checks = (accounts * txs_with_outputs) as f64;
        println!(
            "{n:>2} thread(s): {accounts} accounts x {txs_with_outputs} txs in {secs:.2}s = {:.0} account-tx checks/s ({found} matches)",
            checks / secs
        );
    }
}
