//! Store behaviour, run against SQLite always and against PostgreSQL when
//! `LWS_TEST_POSTGRES_URL` points at an empty, disposable database.

use super::*;
use lws_core::output_flags;

async fn backends() -> Vec<Store> {
    let mut stores = Vec::new();
    let path = std::env::temp_dir().join(format!(
        "lws-store-test-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let sqlite =
        Store::connect(&format!("sqlite:{}", path.display()), 4, Sealer::new((1, &[42; 32]), &[])).await.unwrap();
    sqlite.migrate().await.unwrap();
    stores.push(sqlite);

    if let Ok(url) = std::env::var("LWS_TEST_POSTGRES_URL") {
        // A fresh database per test, so tests can run in parallel.
        let name = format!("lws_test_{}", path.file_stem().unwrap().to_string_lossy().replace('-', "_"));
        let admin = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}")).execute(&admin).await.unwrap();
        admin.close().await;
        let (base, _) = url.rsplit_once('/').unwrap();
        let pg = Store::connect(&format!("{base}/{name}"), 4, Sealer::new((1, &[42; 32]), &[])).await.unwrap();
        pg.migrate().await.unwrap();
        stores.push(pg);
    }
    stores
}

fn new_account(n: u8, start: u64) -> NewAccount {
    NewAccount {
        address: format!("address-{n}"),
        view_public: [n; 32],
        spend_public: [n.wrapping_add(100); 32],
        view_key: ViewKey::new([n.wrapping_add(200); 32]),
        status: AccountStatus::Active,
        flags: 0,
        start_height: start,
    }
}

fn block(height: u64, fork: u8) -> BlockId {
    let mut hash = [fork; 32];
    hash[..8].copy_from_slice(&height.to_le_bytes());
    BlockId { height, hash }
}

fn output(account: AccountId, height: u64, global_index: u64, amount: u64) -> Output {
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
        pub_key: {
            let mut k = [4; 32];
            k[..8].copy_from_slice(&global_index.to_le_bytes());
            k
        },
        unlock_time: u64::MAX,
        timestamp: 1_700_000_000,
        mixin: 15,
        flags: output_flags::RINGCT,
        payment_id: Some(vec![9; 8]),
        locked_key_image: None,
        token: None,
        rct: Some(vec![5; 96]),
    }
}

fn spend(account: AccountId, height: u64, source: u64) -> Spend {
    Spend {
        account_id: account,
        height,
        tx_hash: [height as u8; 32],
        key_image: [source as u8; 32],
        source: OutputRef { amount: 0, global_index: source },
        timestamp: 1_700_000_100,
        unlock_time: 0,
        mixin: 15,
        payment_id: None,
    }
}

async fn created(store: &Store, n: u8, start: u64) -> Account {
    match store.create_account(&new_account(n, start)).await.unwrap() {
        CreateOutcome::Created(a) => a,
        CreateOutcome::Exists(_) => panic!("account {n} already existed"),
    }
}

#[tokio::test]
async fn migrations_are_idempotent_and_network_is_pinned() {
    for store in backends().await {
        store.migrate().await.unwrap();
        store.check_network(Network::Testnet).await.unwrap();
        store.check_network(Network::Testnet).await.unwrap();
        let err = store.check_network(Network::Mainnet).await.unwrap_err();
        assert!(matches!(err, Error::NetworkMismatch { .. }), "{}", store.backend());
    }
}

#[tokio::test]
async fn accounts_create_once() {
    for store in backends().await {
        let a = created(&store, 1, 50).await;
        assert_eq!((a.start_height, a.scan_height, a.generation), (50, 50, 0));
        assert!(
            matches!(store.create_account(&new_account(1, 99)).await.unwrap(), CreateOutcome::Exists(e) if e.id == a.id)
        );
        let by_addr = store.account_by_address("address-1").await.unwrap().unwrap();
        assert_eq!(by_addr.view_key, ViewKey::new([201; 32]));
        assert!(store.account_by_address("nope").await.unwrap().is_none());

        assert_eq!(store.set_status(&["address-1".into()], AccountStatus::Hidden).await.unwrap(), vec!["address-1"]);
        assert!(store.scan_targets(None).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn commit_is_atomic_and_not_replayable() {
    for store in backends().await {
        let a = created(&store, 1, 10).await;
        let epoch = store.chain_epoch().await.unwrap();
        let commit = ScanCommit {
            epoch,
            blocks: (10..=12).map(|h| block(h, 0)).collect(),
            accounts: vec![AccountProgress { id: a.id, generation: 0, from_height: 10, to_height: 12 }],
            outputs: vec![output(a.id, 11, 500, u64::MAX), output(a.id, 12, 501, 7)],
            spends: vec![spend(a.id, 12, 500)],
        };
        let report = store.commit_scan(&commit).await.unwrap();
        assert_eq!(report.accepted, vec![a.id]);
        assert_eq!((report.outputs, report.spends), (2, 1));

        let outs = store.outputs(a.id, 0).await.unwrap();
        assert_eq!(outs.len(), 2);
        assert_eq!(outs[0], output(a.id, 11, 500, u64::MAX), "{}: u64 values survive", store.backend());
        assert_eq!(store.outputs(a.id, 12).await.unwrap().len(), 1, "min_height is inclusive");
        assert_eq!(store.spends(a.id, 0).await.unwrap(), vec![spend(a.id, 12, 500)]);
        assert_eq!(store.account_by_id(a.id).await.unwrap().unwrap().scan_height, 12);

        // Replaying the same batch must not move anything or duplicate rows.
        let replay = store.commit_scan(&commit).await.unwrap();
        assert_eq!(replay.rejected, vec![a.id]);
        assert_eq!(store.outputs(a.id, 0).await.unwrap().len(), 2);
        assert_eq!(store.owned_refs(a.id).await.unwrap().len(), 2);
        let refs = [OutputRef { amount: 0, global_index: 501 }, OutputRef { amount: 0, global_index: 999 }];
        assert_eq!(store.owners_of(&refs).await.unwrap(), vec![(a.id, refs[0])]);
    }
}

#[tokio::test]
async fn rollback_discards_the_abandoned_branch() {
    for store in backends().await {
        let a = created(&store, 1, 0).await;
        let b = created(&store, 2, 0).await;
        let epoch = store.chain_epoch().await.unwrap();
        store
            .commit_scan(&ScanCommit {
                epoch,
                blocks: (0..=20).map(|h| block(h, 0)).collect(),
                accounts: vec![
                    AccountProgress { id: a.id, generation: 0, from_height: 0, to_height: 20 },
                    AccountProgress { id: b.id, generation: 0, from_height: 0, to_height: 20 },
                ],
                outputs: vec![output(a.id, 5, 1, 1), output(a.id, 18, 2, 2)],
                spends: vec![spend(a.id, 19, 1)],
            })
            .await
            .unwrap();

        let report = store.rollback_to(15).await.unwrap();
        assert_eq!(report, RollbackReport { accounts: 2, outputs: 1, spends: 1 });
        let a2 = store.account_by_id(a.id).await.unwrap().unwrap();
        assert_eq!((a2.scan_height, a2.generation), (15, 1));
        assert_eq!(store.tip().await.unwrap().unwrap().height, 15);
        assert_eq!(store.outputs(a.id, 0).await.unwrap().len(), 1);

        // A batch fetched before the rollback is refused outright.
        let stale = ScanCommit {
            epoch,
            blocks: vec![block(16, 0)],
            accounts: vec![AccountProgress { id: b.id, generation: 1, from_height: 15, to_height: 16 }],
            ..Default::default()
        };
        assert!(matches!(store.commit_scan(&stale).await, Err(Error::EpochChanged)));

        // So is one whose blocks disagree with what is stored.
        let epoch = store.chain_epoch().await.unwrap();
        let forked = ScanCommit { epoch, blocks: vec![block(15, 9), block(16, 9)], ..stale };
        assert!(matches!(store.commit_scan(&forked).await, Err(Error::ChainConflict(15))));

        // Stale generation: the account is skipped, the batch is not.
        let ok = ScanCommit {
            epoch,
            blocks: vec![block(16, 0)],
            accounts: vec![
                AccountProgress { id: a.id, generation: 0, from_height: 15, to_height: 16 },
                AccountProgress { id: b.id, generation: 1, from_height: 15, to_height: 16 },
            ],
            outputs: vec![output(a.id, 16, 3, 3), output(b.id, 16, 4, 4)],
            spends: vec![],
        };
        let report = store.commit_scan(&ok).await.unwrap();
        assert_eq!((report.accepted, report.rejected), (vec![b.id], vec![a.id]));
        assert_eq!(store.outputs(a.id, 16).await.unwrap().len(), 0);
        assert_eq!(store.outputs(b.id, 16).await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn rescan_resets_one_account() {
    for store in backends().await {
        let a = created(&store, 1, 0).await;
        let epoch = store.chain_epoch().await.unwrap();
        store
            .commit_scan(&ScanCommit {
                epoch,
                blocks: vec![block(9, 0)],
                accounts: vec![AccountProgress { id: a.id, generation: 0, from_height: 0, to_height: 9 }],
                outputs: vec![output(a.id, 3, 1, 1), output(a.id, 8, 2, 2)],
                spends: vec![],
            })
            .await
            .unwrap();
        assert_eq!(store.rescan(&["address-1".into(), "missing".into()], 4).await.unwrap(), vec!["address-1"]);
        let a2 = store.account_by_id(a.id).await.unwrap().unwrap();
        assert_eq!((a2.start_height, a2.scan_height, a2.generation), (4, 4, 1));
        assert_eq!(store.outputs(a.id, 0).await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn chain_sample_is_dense_then_sparse() {
    for store in backends().await {
        store.insert_blocks(&(0..=100).map(|h| block(h, 0)).collect::<Vec<_>>()).await.unwrap();
        let heights: Vec<u64> = store.chain_sample(100).await.unwrap().iter().map(|b| b.height).collect();
        assert_eq!(&heights[..10], &[100, 99, 98, 97, 96, 95, 94, 93, 92, 91]);
        assert_eq!(*heights.last().unwrap(), 0);
        assert!(heights.windows(2).all(|w| w[0] > w[1]));
        assert!(heights.len() < 20);
    }
}

#[tokio::test]
async fn requests_and_leases() {
    for store in backends().await {
        let request = Request {
            kind: RequestKind::Create,
            address: "address-7".into(),
            view_public: [7; 32],
            spend_public: [107; 32],
            view_key: ViewKey::new([207; 32]),
            start_height: 33,
            flags: lws_core::account_flags::GENERATED_LOCALLY,
            created_at: 1,
        };
        assert!(store.add_request(&request).await.unwrap());
        assert!(!store.add_request(&request).await.unwrap());
        assert_eq!(store.list_requests().await.unwrap().len(), 1);
        assert_eq!(store.accept_requests(RequestKind::Create, &["address-7".into()]).await.unwrap(), vec!["address-7"]);
        let account = store.account_by_address("address-7").await.unwrap().unwrap();
        assert_eq!((account.start_height, account.spend_public), (33, [107; 32]));
        assert!(account.generated_locally());
        assert!(store.list_requests().await.unwrap().is_empty());

        let ttl = Duration::from_secs(30);
        assert!(store.acquire_lease("scanner-0", "a", ttl).await.unwrap());
        assert!(store.acquire_lease("scanner-0", "a", ttl).await.unwrap());
        assert!(!store.acquire_lease("scanner-0", "b", ttl).await.unwrap());
        store.release_lease("scanner-0", "a").await.unwrap();
        assert!(store.acquire_lease("scanner-0", "b", ttl).await.unwrap());
    }
}

#[tokio::test]
async fn view_keys_are_sealed_at_rest() {
    for store in backends().await {
        let a = created(&store, 3, 0).await;
        let raw = dispatch!(store, pool => {
            sqlx::query("SELECT view_key FROM accounts WHERE id = $1").bind(a.id).fetch_one(pool).await.unwrap().bytes("view_key").unwrap()
        });
        assert_ne!(raw.len(), 32, "{}: stored in plaintext", store.backend());
        assert_eq!(store.account_by_id(a.id).await.unwrap().unwrap().view_key, ViewKey::new([203; 32]));
    }
}

#[tokio::test]
async fn reseal_rotates_every_key() {
    let path = std::env::temp_dir().join(format!("lws-store-reseal-{}.db", std::process::id()));
    let url = format!("sqlite:{}", path.display());
    let old = Store::connect(&url, 2, Sealer::new((1, &[1; 32]), &[])).await.unwrap();
    old.migrate().await.unwrap();
    let a = created(&old, 4, 0).await;

    // Rotate: new key 2, old key 1 still accepted for reading.
    let rotating = Store::connect(&url, 2, Sealer::new((2, &[2; 32]), &[(1, [1; 32])])).await.unwrap();
    assert_eq!(rotating.reseal_all().await.unwrap(), 1);

    // Key 1 can now be retired entirely.
    let new = Store::connect(&url, 2, Sealer::new((2, &[2; 32]), &[])).await.unwrap();
    assert_eq!(new.account_by_id(a.id).await.unwrap().unwrap().view_key, ViewKey::new([204; 32]));
    assert!(old.account_by_id(a.id).await.is_err(), "old key alone can no longer open it");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn a_wrong_sealing_key_is_refused_at_startup() {
    let path = std::env::temp_dir().join(format!("lws-store-sealcheck-{}.db", std::process::id()));
    let url = format!("sqlite:{}", path.display());
    let first = Store::connect(&url, 2, Sealer::new((1, &[1; 32]), &[])).await.unwrap();
    first.migrate().await.unwrap();
    created(&first, 5, 0).await;
    first.check_sealing_key().await.unwrap();
    first.check_sealing_key().await.unwrap();

    let other = Store::connect(&url, 2, Sealer::new((1, &[9; 32]), &[])).await.unwrap();
    assert!(matches!(other.check_sealing_key().await, Err(Error::WrongSealingKey(_))));
    let none = Store::connect(&url, 2, Sealer::plaintext()).await.unwrap();
    assert!(matches!(none.check_sealing_key().await, Err(Error::WrongSealingKey(_))));

    // After a rotation the new key alone passes.
    let rotating = Store::connect(&url, 2, Sealer::new((2, &[2; 32]), &[(1, [1; 32])])).await.unwrap();
    rotating.check_sealing_key().await.unwrap();
    rotating.reseal_all().await.unwrap();
    let rotated = Store::connect(&url, 2, Sealer::new((2, &[2; 32]), &[])).await.unwrap();
    rotated.check_sealing_key().await.unwrap();
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn databases_from_before_the_check_are_verified_by_their_accounts() {
    let path = std::env::temp_dir().join(format!("lws-store-sealcheck-old-{}.db", std::process::id()));
    let url = format!("sqlite:{}", path.display());
    let first = Store::connect(&url, 2, Sealer::new((1, &[1; 32]), &[])).await.unwrap();
    first.migrate().await.unwrap();
    created(&first, 6, 0).await;
    let wrong = Store::connect(&url, 2, Sealer::new((1, &[9; 32]), &[])).await.unwrap();
    assert!(wrong.check_sealing_key().await.is_err());
    first.check_sealing_key().await.unwrap();
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn account_pages_search_and_counts() {
    for store in backends().await {
        for n in 1..=25 {
            created(&store, n, u64::from(n) * 10).await;
        }
        store.set_status(&["address-3".into(), "address-4".into()], AccountStatus::Inactive).await.unwrap();
        store.set_status(&["address-5".into()], AccountStatus::Hidden).await.unwrap();

        let first = store.account_page(&AccountFilter { limit: 10, ..Default::default() }).await.unwrap();
        assert_eq!(first.len(), 10);
        assert_eq!(first[0].address, "address-25", "newest first");
        let second = store
            .account_page(&AccountFilter { limit: 10, before_id: Some(first[9].id), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(second[0].id, first[9].id - 1);

        let inactive = store
            .account_page(&AccountFilter { status: Some(AccountStatus::Inactive), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(inactive.len(), 2);

        // Prefix search, with LIKE wildcards taken literally.
        let found = store
            .account_page(&AccountFilter { search: Some("address-2".into()), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(found.len(), 7, "address-2 and address-20..25");
        let none = store
            .account_page(&AccountFilter { search: Some("address_%".into()), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert!(none.is_empty());

        let behind = store
            .account_page(&AccountFilter { scanned_below: Some(50), limit: 50, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(behind.len(), 4, "start heights 10..40");

        let c = store.account_counts(100).await.unwrap();
        assert_eq!((c.active, c.inactive, c.hidden), (22, 2, 1));
        assert_eq!(c.active_behind, 6, "active accounts scanned below 100: 1, 2, 6..9");

        let one = store.account_summary("address-7").await.unwrap().unwrap();
        assert_eq!(one.scan_height, 70);
        assert_eq!(store.account_activity(one.id).await.unwrap(), (0, 0));
    }
}

#[tokio::test]
async fn audit_trail_and_instances() {
    for store in backends().await {
        store.record_audit("token", "127.0.0.1", "rescan", "{\"height\":5}").await.unwrap();
        store.record_audit("cli:root", "local", "set_status", "{}").await.unwrap();
        let log = store.audit_log(None, 10).await.unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].action, "set_status");
        assert_eq!(store.audit_log(Some(log[0].id), 10).await.unwrap().len(), 1);
        assert_eq!(store.prune_audit(lws_core::now_secs() + 1).await.unwrap(), 2);

        store.heartbeat("api-1", "api", "0.1.0", 100, "{\"x\":1}").await.unwrap();
        store.heartbeat("api-1", "api", "0.1.0", 100, "{\"x\":2}").await.unwrap();
        store.heartbeat("scan-1", "scanner", "0.1.0", 100, "{}").await.unwrap();
        let live = store.instances(0).await.unwrap();
        assert_eq!(live.len(), 2);
        assert_eq!(live[0].stats, "{\"x\":2}");
        store.remove_instances(0, Some("api-1")).await.unwrap();
        assert_eq!(store.instances(0).await.unwrap().len(), 1);
        assert!(store.instances(lws_core::now_secs() + 10).await.unwrap().is_empty());

        assert!(store.acquire_lease("scanner-0-of-1", "scan-1", Duration::from_secs(30)).await.unwrap());
        assert_eq!(store.leases().await.unwrap()[0].holder, "scan-1");
    }
}
