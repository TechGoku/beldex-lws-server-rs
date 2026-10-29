//! The chain scanner.
//!
//! Accounts are grouped into *lanes* of nearby scan heights. Each pass runs
//! the tip lane (accounts that are caught up, i.e. almost everyone) plus the
//! most-behind lanes concurrently, so a wallet importing from genesis never
//! delays balance updates for everyone else. Lanes merge naturally as the
//! slower ones catch up, because grouping is recomputed every pass.
//!
//! Within a lane, block downloads run ahead of scanning: up to `fetch_window`
//! requests are in flight at once, spread across every configured daemon, and
//! their batches are applied strictly in height order while the CPU pool scans.
//! Scanning a batch is split across all cores by account.
//!
//! Every commit is atomic and checked by the store against the chain epoch and
//! each account's generation, so a reorg, an operator rescan or a second
//! scanner process can never interleave with a batch and leave wrong rows.
//! Several scanner processes can share one database: each owns a shard of the
//! account ids through a lease, and a standby takes over a shard when its
//! holder stops renewing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use lws_core::metrics::Metrics;
use lws_core::{output_flags, Account, AccountId, BlockId, Output, OutputRef, Spend, TokenData};
use lws_daemon::{Blocks, Daemon, GetBlocks};
use lws_kernel::{Batch, BlockEntry, ScanKey};
use lws_store::{AccountProgress, ScanCommit, Store};
use rayon::prelude::*;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

#[derive(Clone, Debug)]
pub struct Config {
    /// The daemon must report this network or the scanner refuses to run:
    /// another chain would look like a deep reorg and roll accounts back.
    pub network: lws_core::Network,
    /// `(index, count)`: scan only accounts with `id % count == index`.
    pub shard: Option<(u32, u32)>,
    /// Ask the daemon for pruned transactions (no signatures or range proofs).
    pub prune: bool,
    /// How long to wait at the chain tip before polling again.
    pub poll_interval: Duration,
    /// Accounts within this many blocks of each other scan as one lane.
    pub lane_window: u64,
    /// Lanes scanned concurrently in one pass.
    pub max_lanes: usize,
    /// Batches a lane scans before lanes are regrouped.
    pub batches_per_pass: usize,
    /// Block requests a lane keeps in flight at once, rotated across daemons.
    pub fetch_window: usize,
    /// Accounts per kernel call; the unit of CPU parallelism.
    pub accounts_per_task: usize,
    /// Screen accounts with the fast Rust prefilter and run the kernel only
    /// on the ones it flags. Off means every account goes through the kernel.
    pub fast_prefilter: bool,
    pub lease_ttl: Duration,
    /// Identifies this process as a lease holder.
    pub instance: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            network: lws_core::Network::Mainnet,
            shard: None,
            prune: true,
            poll_interval: Duration::from_secs(5),
            lane_window: 5_000,
            max_lanes: 4,
            batches_per_pass: 20,
            fetch_window: 4,
            accounts_per_task: 256,
            fast_prefilter: true,
            lease_ttl: Duration::from_secs(30),
            instance: format!("pid-{}", std::process::id()),
        }
    }
}

pub struct Scanner {
    store: Arc<Store>,
    daemon: Arc<Daemon>,
    metrics: Arc<Metrics>,
    config: Config,
    pool: Arc<rayon::ThreadPool>,
    /// New-block announcements; wakes the scanner instead of waiting for
    /// the next poll.
    blocks: Option<watch::Receiver<u64>>,
}

/// A batch after kernel parsing and scanning, as plain owned data.
struct Scanned {
    blocks: Vec<BlockId>,
    first_prev: [u8; 32],
    /// Owned outputs, in chain order, with the index of their tx in the batch.
    outputs: Vec<(usize, Output)>,
    /// One entry per ring member of every input, in chain order.
    ring_members: Vec<RingMember>,
}

struct RingMember {
    tx: usize,
    height: u64,
    timestamp: u64,
    tx_hash: [u8; 32],
    key_image: [u8; 32],
    source: OutputRef,
    unlock_time: u64,
    mixin: u32,
    payment_id: Option<Vec<u8>>,
}

enum Fetched {
    Blocks(Blocks),
    /// The daemon has nothing at or after the requested height yet.
    AtTip,
}

/// Keeps several block requests in flight for one lane, each starting at a
/// different daemon, and hands batches back strictly in height order.
///
/// Requests are speculative: the next one starts where the previous is
/// expected to end, using the batch size the daemons have been returning.
/// A short answer (the daemon capped it by size) leaves a gap, which is
/// requested before anything after it is handed out; an answer that starts
/// below the height asked for is dropped.
struct Prefetcher {
    daemon: Arc<Daemon>,
    metrics: Arc<Metrics>,
    prune: bool,
    lane: usize,
    window: usize,
    inflight: VecDeque<(u64, tokio::task::JoinHandle<anyhow::Result<Fetched>>)>,
    next_issue: u64,
    stride: u64,
    tip: u64,
    issued: usize,
    budget: usize,
}

impl Prefetcher {
    fn issue(&mut self, start: u64) -> tokio::task::JoinHandle<anyhow::Result<Fetched>> {
        let request = GetBlocks { start_height: start, prune: self.prune, ..Default::default() };
        let lane = self.lane + self.issued;
        self.issued += 1;
        self.budget = self.budget.saturating_sub(1);
        tokio::spawn(Scanner::fetch(self.daemon.clone(), self.metrics.clone(), request, lane))
    }

    fn fill(&mut self) {
        while self.inflight.len() < self.window && self.budget > 0 && self.next_issue < self.tip {
            let start = self.next_issue;
            let task = self.issue(start);
            self.inflight.push_back((start, task));
            self.next_issue += self.stride.max(1);
        }
    }

    /// The batch starting exactly at `height`, or `None` when no daemon has it
    /// yet or this pass's budget is spent.
    async fn next(&mut self, height: u64) -> anyhow::Result<Option<Blocks>> {
        while self.inflight.front().is_some_and(|(s, _)| *s < height) {
            if let Some((_, task)) = self.inflight.pop_front() {
                task.abort();
            }
        }
        if self.inflight.front().is_none_or(|(s, _)| *s > height) {
            if self.budget == 0 {
                return Ok(None);
            }
            let task = self.issue(height);
            self.inflight.push_front((height, task));
        }
        let (start, task) = self.inflight.pop_front().expect("a request for `height` was just ensured");
        match task.await.context("fetch task")?? {
            Fetched::AtTip => {
                self.abort();
                Ok(None)
            }
            Fetched::Blocks(blocks) => {
                if blocks.start_height != start {
                    bail!("daemon answered from height {}, asked for {start}", blocks.start_height);
                }
                self.tip = self.tip.max(blocks.current_height);
                if blocks.blocks.is_empty() {
                    return Ok(None);
                }
                let end = start + blocks.blocks.len() as u64;
                if end < self.tip {
                    self.stride = blocks.blocks.len() as u64;
                }
                self.next_issue = self.next_issue.max(end);
                self.fill();
                Ok(Some(blocks))
            }
        }
    }

    fn abort(&mut self) {
        for (_, task) in self.inflight.drain(..) {
            task.abort();
        }
    }
}

pub fn lease_name(shard: Option<(u32, u32)>) -> String {
    let (index, count) = shard.unwrap_or((0, 1));
    format!("scanner-{index}-of-{count}")
}

/// Groups accounts (sorted by scan height) into lanes no wider than `window`.
fn make_lanes(mut accounts: Vec<Account>, window: u64) -> Vec<Vec<Account>> {
    accounts.sort_by_key(|a| (a.scan_height, a.id));
    let mut lanes: Vec<Vec<Account>> = Vec::new();
    for account in accounts {
        match lanes.last_mut() {
            Some(lane) if account.scan_height - lane[0].scan_height <= window => lane.push(account),
            _ => lanes.push(vec![account]),
        }
    }
    lanes
}

/// The tip lane first, then the furthest-behind lanes, up to `max`.
fn pick_lanes(mut lanes: Vec<Vec<Account>>, max: usize) -> Vec<Vec<Account>> {
    if lanes.len() <= max {
        lanes.reverse();
        return lanes;
    }
    let tip = lanes.pop().unwrap();
    let mut picked = vec![tip];
    picked.extend(lanes.into_iter().take(max.saturating_sub(1)));
    picked
}

impl Scanner {
    pub fn new(
        store: Arc<Store>,
        daemon: Arc<Daemon>,
        metrics: Arc<Metrics>,
        config: Config,
        threads: usize,
    ) -> anyhow::Result<Scanner> {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .thread_name(|i| format!("lws-scan-{i}"))
            .build()
            .context("building scan thread pool")?;
        Ok(Scanner { store, daemon, metrics, config, pool: Arc::new(pool), blocks: None })
    }

    /// Wake on new-block announcements (see `lws_daemon::notify`) as well as
    /// on the poll interval.
    pub fn with_block_notifications(mut self, blocks: watch::Receiver<u64>) -> Scanner {
        self.blocks = Some(blocks);
        self
    }

    /// Makes sure the genesis hash is stored: every `get_blocks.bin` block-id
    /// list must end with it.
    pub async fn ensure_genesis(&self) -> anyhow::Result<()> {
        if self.store.block_hash(0).await?.is_none() {
            let hash = self.daemon.block_hash(0).await.context("fetching genesis block hash")?;
            self.store.insert_blocks(&[BlockId { height: 0, hash }]).await?;
        }
        Ok(())
    }

    /// Waits for a reachable daemon and checks it is on our network.
    async fn check_daemon(&self, shutdown: &mut watch::Receiver<bool>) -> anyhow::Result<bool> {
        let mut delay = Duration::from_secs(1);
        loop {
            match self.daemon.get_info().await {
                Ok(info) if info.nettype == self.config.network.daemon_name() => return Ok(true),
                Ok(info) => bail!(
                    "refusing to scan: daemon is on '{}' but this server is configured for '{}'",
                    info.nettype,
                    self.config.network
                ),
                Err(e) => warn!(error = %e, "daemon not reachable yet"),
            }
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = shutdown.changed() => return Ok(false),
            }
            delay = (delay * 2).min(Duration::from_secs(30));
        }
    }

    pub async fn run(&self, mut shutdown: watch::Receiver<bool>) -> anyhow::Result<()> {
        lws_kernel::self_check()?;
        if !self.check_daemon(&mut shutdown).await? {
            return Ok(());
        }
        self.daemon.refresh_heights().await;
        self.ensure_genesis().await?;
        let lease = lease_name(self.config.shard);
        let mut holding = false;
        let mut blocks = self.blocks.clone();
        if let Some(rx) = blocks.as_mut() {
            rx.borrow_and_update();
        }
        let mut failures = 0u32;
        info!(%lease, instance = %self.config.instance, "scanner started");

        while !*shutdown.borrow() {
            let have = self.store.acquire_lease(&lease, &self.config.instance, self.config.lease_ttl).await?;
            if have != holding {
                holding = have;
                if have {
                    info!(%lease, "lease acquired, scanning");
                } else {
                    warn!(%lease, "lease held elsewhere, standing by");
                }
            }
            let wait = if !have {
                self.config.lease_ttl / 3
            } else {
                match self.pass().await {
                    Ok(true) => {
                        failures = 0;
                        Duration::ZERO
                    }
                    Ok(false) => {
                        failures = 0;
                        self.config.poll_interval
                    }
                    Err(e) => {
                        failures += 1;
                        Metrics::add(&self.metrics.scan_errors_total, 1);
                        error!(error = %format!("{e:#}"), failures, "scan pass failed");
                        Duration::from_secs((1u64 << failures.min(6)).min(60))
                    }
                }
            };
            if !wait.is_zero() {
                // One receiver for the whole run: it remembers which
                // announcement it has seen, so each block wakes us once.
                let announced = async {
                    match blocks.as_mut() {
                        Some(rx) => {
                            let _ = rx.changed().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = shutdown.changed() => {}
                    _ = announced => debug!("woken by a new block"),
                }
            }
        }
        if holding {
            self.store.release_lease(&lease, &self.config.instance).await?;
        }
        info!("scanner stopped");
        Ok(())
    }

    /// One pass over every lane. Returns whether anything was committed.
    pub async fn pass(&self) -> anyhow::Result<bool> {
        Metrics::add(&self.metrics.scan_passes_total, 1);
        let accounts = self.store.scan_targets(self.config.shard).await?;
        Metrics::set(&self.metrics.scanned_accounts, accounts.len() as u64);
        if accounts.is_empty() {
            let info = self.daemon.get_info().await?;
            Metrics::set(&self.metrics.daemon_height, info.height);
            return Ok(false);
        }
        Metrics::set(&self.metrics.min_scan_height, accounts.iter().map(|a| a.scan_height).min().unwrap_or(0));
        let lanes = make_lanes(accounts, self.config.lane_window);
        Metrics::set(&self.metrics.scan_lanes, lanes.len() as u64);
        let picked = pick_lanes(lanes, self.config.max_lanes.max(1));

        let results =
            futures::future::join_all(picked.into_iter().enumerate().map(|(i, lane)| self.scan_lane(i, lane))).await;
        let mut progressed = false;
        let mut first_error = None;
        for result in results {
            match result {
                Ok(p) => progressed |= p,
                Err(e) => {
                    warn!(error = %format!("{e:#}"), "lane failed");
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            Some(e) if !progressed => Err(e),
            _ => Ok(progressed),
        }
    }

    async fn fetch(
        daemon: Arc<Daemon>,
        metrics: Arc<Metrics>,
        request: GetBlocks,
        lane: usize,
    ) -> anyhow::Result<Fetched> {
        let began = Instant::now();
        let result = daemon.get_blocks(&request, lane).await;
        metrics.daemon_fetch_duration_seconds.observe(began.elapsed());
        match result {
            Ok(blocks) => Ok(Fetched::Blocks(blocks)),
            // An explicit start beyond the daemon's tip is refused with a
            // failed status; it only means there is nothing new yet.
            Err(lws_daemon::Error::Status(_)) if request.block_ids.is_empty() => Ok(Fetched::AtTip),
            Err(e) => Err(e.into()),
        }
    }

    async fn scan_lane(&self, lane_no: usize, mut accounts: Vec<Account>) -> anyhow::Result<bool> {
        let start = accounts.iter().map(|a| a.scan_height).min().unwrap_or(0);
        let epoch = self.store.chain_epoch().await?;
        let sample = self.store.chain_sample(start).await?;
        let by_ids = sample.first().map(|b| b.height) == Some(start) && sample.last().map(|b| b.height) == Some(0);
        let request = if by_ids {
            GetBlocks {
                block_ids: sample.iter().map(|b| b.hash).collect(),
                prune: self.config.prune,
                ..Default::default()
            }
        } else {
            GetBlocks { start_height: start, prune: self.config.prune, ..Default::default() }
        };

        let mut first_fetch =
            Some(tokio::spawn(Self::fetch(self.daemon.clone(), self.metrics.clone(), request, lane_no)));
        let mut ahead: Option<Prefetcher> = None;
        let mut parent: Option<BlockId> = by_ids.then(|| sample[0]);
        let mut first = true;
        let mut progressed = false;

        for _ in 0..self.config.batches_per_pass.max(1) {
            let fetched = match (first_fetch.take(), ahead.as_mut(), parent) {
                (Some(task), _, _) => match task.await.context("fetch task")?? {
                    Fetched::AtTip => None,
                    Fetched::Blocks(b) => Some(b),
                },
                (None, Some(pre), Some(p)) => pre.next(p.height + 1).await?,
                _ => None,
            };
            let Some(blocks) = fetched else { break };
            Metrics::set(&self.metrics.daemon_height, blocks.current_height);

            if first && by_ids && blocks.start_height < start {
                // The daemon's chain left ours below `start`.
                let report = self.store.rollback_to(blocks.start_height).await?;
                Metrics::add(&self.metrics.reorgs_total, 1);
                warn!(
                    fork = blocks.start_height,
                    from = start,
                    accounts = report.accounts,
                    outputs = report.outputs,
                    spends = report.spends,
                    "chain reorganisation, rolled back"
                );
                return Ok(true);
            }
            if blocks.blocks.is_empty() {
                break;
            }
            let batch_first = blocks.start_height;
            let batch_last = batch_first + blocks.blocks.len() as u64 - 1;
            let expected_first = parent.map_or(start, |p| if first { p.height } else { p.height + 1 });
            if batch_first != expected_first {
                bail!("daemon answered from height {batch_first}, expected {expected_first}");
            }
            if batch_last <= start && first {
                break; // only the block we already have: at the tip
            }

            // Start downloading what follows before scanning this batch.
            if ahead.is_none() {
                let mut pre = Prefetcher {
                    daemon: self.daemon.clone(),
                    metrics: self.metrics.clone(),
                    prune: self.config.prune,
                    lane: lane_no,
                    window: self.config.fetch_window.max(1),
                    inflight: VecDeque::new(),
                    next_issue: batch_last + 1,
                    stride: blocks.blocks.len() as u64,
                    tip: blocks.current_height,
                    issued: 1,
                    budget: self.config.batches_per_pass.saturating_sub(1),
                };
                pre.fill();
                ahead = Some(pre);
            }

            let keys: Vec<ScanKey> = accounts
                .iter()
                .map(|a| ScanKey {
                    view_secret: *a.view_key.bytes(),
                    spend_public: a.spend_public,
                    scan_height: a.scan_height,
                })
                .collect();
            let ids: Vec<AccountId> = accounts.iter().map(|a| a.id).collect();
            let batch_began = Instant::now();
            let began = batch_began;
            let scanned = {
                let pool = self.pool.clone();
                let per_task = self.config.accounts_per_task.max(1);
                let pruned = self.config.prune;
                let fast = self.config.fast_prefilter;
                tokio::task::spawn_blocking(move || scan_batch(&pool, &blocks, &keys, &ids, per_task, pruned, fast))
                    .await
                    .context("scan task")??
            };
            Metrics::add(&self.metrics.scan_cpu_ms_total, began.elapsed().as_millis() as u64);

            // Continuity: the batch must extend the block we resumed from.
            if let Some(p) = parent {
                let ok =
                    if first && by_ids { scanned.blocks.first() == Some(&p) } else { scanned.first_prev == p.hash };
                if !ok {
                    warn!(height = p.height, "batch does not extend the previous block; regrouping");
                    break;
                }
            }

            let began = Instant::now();
            let commit = self.build_commit(epoch, &accounts, scanned).await?;
            let build_ms = began.elapsed().as_millis() as u64;
            let found = (commit.outputs.len() as u64, commit.spends.len() as u64);
            let began = Instant::now();
            let committed = self.store.commit_scan(&commit).await;
            debug!(build_ms, commit_ms = began.elapsed().as_millis() as u64, "batch db");
            self.metrics.scan_batch_duration_seconds.observe(batch_began.elapsed());
            match committed {
                Ok(report) => {
                    Metrics::add(&self.metrics.blocks_scanned_total, commit.blocks.len() as u64);
                    Metrics::add(&self.metrics.outputs_found_total, report.outputs);
                    Metrics::add(&self.metrics.spends_found_total, report.spends);
                    Metrics::add(&self.metrics.commits_rejected_total, report.rejected.len() as u64);
                    debug!(
                        lane = lane_no,
                        from = batch_first,
                        to = batch_last,
                        accounts = accounts.len(),
                        outputs = found.0,
                        spends = found.1,
                        rejected = report.rejected.len(),
                        "batch committed"
                    );
                    accounts.retain(|a| report.accepted.contains(&a.id));
                    for a in &mut accounts {
                        a.scan_height = a.scan_height.max(batch_last);
                    }
                    progressed = true;
                }
                Err(lws_store::Error::EpochChanged) => {
                    debug!(lane = lane_no, "reorg handled elsewhere during batch; regrouping");
                    break;
                }
                Err(lws_store::Error::ChainConflict(height)) => {
                    // The daemon's current chain disagrees with a stored hash:
                    // the stored branch is stale.
                    let report = self.store.rollback_to(height.saturating_sub(1)).await?;
                    Metrics::add(&self.metrics.reorgs_total, 1);
                    warn!(height, accounts = report.accounts, "stored chain was stale, rolled back");
                    return Ok(true);
                }
                Err(e) => return Err(e.into()),
            }

            parent = commit.blocks.last().copied();
            first = false;
            if accounts.is_empty() {
                break;
            }
        }
        if let Some(task) = first_fetch {
            task.abort();
        }
        if let Some(mut pre) = ahead {
            pre.abort();
        }
        Ok(progressed)
    }

    /// Turns a scanned batch into a commit: owned outputs, plus candidate
    /// spends found by matching every ring member against outputs already
    /// stored and outputs found earlier in this batch.
    async fn build_commit(&self, epoch: i64, accounts: &[Account], scanned: Scanned) -> anyhow::Result<ScanCommit> {
        let heights: HashMap<AccountId, u64> = accounts.iter().map(|a| (a.id, a.scan_height)).collect();
        let last = scanned.blocks.last().map(|b| b.height).ok_or_else(|| anyhow!("empty batch"))?;

        let unique: Vec<OutputRef> = {
            let set: HashSet<OutputRef> = scanned.ring_members.iter().map(|m| m.source).collect();
            set.into_iter().collect()
        };
        let mut owned: HashMap<OutputRef, AccountId> = self
            .store
            .owners_of(&unique)
            .await?
            .into_iter()
            .filter(|(id, _)| heights.contains_key(id))
            .map(|(id, r)| (r, id))
            .collect();

        let mut spends = Vec::new();
        let mut outputs = scanned.outputs.into_iter().peekable();
        let mut members = scanned.ring_members.into_iter().peekable();
        let mut kept_outputs = Vec::new();
        // Walk txs in order: a tx's inputs see outputs from earlier txs only.
        loop {
            let next_tx = match (members.peek().map(|m| m.tx), outputs.peek().map(|o| o.0)) {
                (None, None) => break,
                (a, b) => a.into_iter().chain(b).min().unwrap(),
            };
            while members.peek().is_some_and(|m| m.tx == next_tx) {
                let m = members.next().unwrap();
                if let Some(&account) = owned.get(&m.source) {
                    if heights.get(&account).is_some_and(|&h| m.height > h) {
                        spends.push(Spend {
                            account_id: account,
                            height: m.height,
                            tx_hash: m.tx_hash,
                            key_image: m.key_image,
                            source: m.source,
                            timestamp: m.timestamp,
                            unlock_time: m.unlock_time,
                            mixin: m.mixin,
                            payment_id: m.payment_id,
                        });
                    }
                }
            }
            while outputs.peek().is_some_and(|o| o.0 == next_tx) {
                let (_, output) = outputs.next().unwrap();
                owned.insert(output.source, output.account_id);
                kept_outputs.push(output);
            }
        }

        Ok(ScanCommit {
            epoch,
            blocks: scanned.blocks,
            accounts: accounts
                .iter()
                .filter(|a| a.scan_height < last)
                .map(|a| AccountProgress {
                    id: a.id,
                    generation: a.generation,
                    from_height: a.scan_height,
                    to_height: last,
                })
                .collect(),
            outputs: kept_outputs,
            spends,
        })
    }
}

/// Parses and scans one batch on the CPU pool. Pure: no I/O.
fn scan_batch(
    pool: &rayon::ThreadPool,
    blocks: &Blocks,
    keys: &[ScanKey],
    ids: &[AccountId],
    per_task: usize,
    pruned: bool,
    fast: bool,
) -> anyhow::Result<Scanned> {
    let entries: Vec<BlockEntry<'_>> = blocks
        .blocks
        .iter()
        .map(|b| BlockEntry {
            block: &b.block,
            txs: b.txs.iter().map(|t| t.as_ref()).collect(),
            output_indices: b.output_indices.clone(),
        })
        .collect();
    let began = Instant::now();
    let batch = Batch::parse(&entries, pruned)?;
    let parse_ms = began.elapsed().as_millis() as u64;
    let view = batch.view();

    // The prefilter screens every (tx, account) pair; the kernel verifies
    // only the pairs it flags, so the cost of a hit is one check, not a
    // rescan of the whole batch for that account.
    let mut matches: Vec<lws_kernel::Match> = if fast {
        let found = pool.install(|| lws_kernel::fast::candidates(&view, keys));
        let mut matches: Vec<lws_kernel::Match> = pool
            .install(|| {
                found
                    .pairs
                    .par_chunks(per_task)
                    .map(|chunk| batch.scan_pairs(keys, chunk))
                    .collect::<Result<Vec<_>, _>>()
            })?
            .into_iter()
            .flatten()
            .collect();
        if !found.full_scan.is_empty() {
            let chosen: Vec<ScanKey> = found.full_scan.iter().map(|&i| keys[i].clone()).collect();
            for mut m in batch.scan(&chosen)? {
                m.account = found.full_scan[m.account as usize] as u32;
                matches.push(m);
            }
        }
        matches
    } else {
        pool.install(|| {
            keys.par_chunks(per_task)
                .enumerate()
                .map(|(chunk, ks)| {
                    batch.scan(ks).map(|mut ms| {
                        for m in &mut ms {
                            m.account += (chunk * per_task) as u32;
                        }
                        ms
                    })
                })
                .collect::<Result<Vec<_>, _>>()
        })?
        .into_iter()
        .flatten()
        .collect()
    };
    matches.sort_by_key(|m| (m.tx, m.output, m.account));
    let scan_ms = began.elapsed().as_millis() as u64;
    let began = Instant::now();

    let mut tx_block = vec![0usize; view.txs.len()];
    for (b, block) in view.blocks.iter().enumerate() {
        for t in block.tx_offset..block.tx_offset + block.tx_count {
            tx_block[t as usize] = b;
        }
    }

    let outputs = matches
        .iter()
        .map(|m| {
            let tx = &view.txs[m.tx as usize];
            let out = &view.outputs[m.output as usize];
            let block = &view.blocks[tx_block[m.tx as usize]];
            let token = (m.flags & lws_kernel::FLAG_TOKEN != 0).then_some(TokenData {
                token_id: m.token_id.data,
                blinded_token_id: out.blinded_token_id.data,
                amount_commitment: out.commitment.data,
                encrypted_amount: out.encrypted_amount,
            });
            let output = Output {
                account_id: ids[m.account as usize],
                height: block.height,
                tx_hash: tx.hash.data,
                tx_prefix_hash: tx.prefix_hash.data,
                tx_pub_key: tx.tx_pub_key.data,
                out_index: out.index,
                // Ring members name pre-ringct outputs by (amount, index) and
                // everything since by index in the amount-0 bucket.
                source: OutputRef {
                    amount: if tx.version >= 2 { 0 } else { out.amount },
                    global_index: out.global_index,
                },
                amount: m.amount,
                mask: m.mask.data,
                pub_key: out.pub_key.data,
                unlock_time: out.unlock_time,
                timestamp: block.timestamp,
                mixin: tx.mixin,
                flags: i16::from(m.flags) & (output_flags::COINBASE | output_flags::RINGCT | output_flags::TOKEN),
                payment_id: m.payment_id().map(<[u8]>::to_vec),
                locked_key_image: (tx.has_locked_key_image != 0).then_some(tx.locked_key_image.data),
                token,
                rct: m.rct().map(|r| r.to_vec()),
            };
            (m.tx as usize, output)
        })
        .collect();

    let mut ring_members = Vec::new();
    for block in view.blocks {
        for (t, tx) in block.txs(&view).iter().enumerate() {
            let tx_index = block.tx_offset as usize + t;
            let long_id = (tx.long_payment_id_len == 32).then(|| tx.long_payment_id.to_vec());
            for input in tx.inputs(&view) {
                for &global_index in input.ring(&view) {
                    ring_members.push(RingMember {
                        tx: tx_index,
                        height: block.height,
                        timestamp: block.timestamp,
                        tx_hash: tx.hash.data,
                        key_image: input.key_image.data,
                        source: OutputRef { amount: input.amount, global_index },
                        unlock_time: tx.unlock_time,
                        mixin: input.ring_count.saturating_sub(1),
                        payment_id: long_id.clone(),
                    });
                }
            }
        }
    }

    debug!(
        blocks = view.blocks.len(),
        parse_ms,
        scan_ms,
        collect_ms = began.elapsed().as_millis() as u64,
        ring_members = ring_members.len(),
        "batch cpu"
    );
    let first_prev = view.blocks.first().map(|b| b.prev_hash.data).unwrap_or_default();
    let blocks_out = view.blocks.iter().map(|b| BlockId { height: b.height, hash: b.hash.data }).collect();
    Ok(Scanned { blocks: blocks_out, first_prev, outputs, ring_members })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lws_core::{AccountStatus, ViewKey};

    fn account(id: i64, height: u64) -> Account {
        Account {
            id,
            address: format!("a{id}"),
            view_public: [0; 32],
            spend_public: [0; 32],
            view_key: ViewKey::new([0; 32]),
            status: AccountStatus::Active,
            flags: 0,
            start_height: height,
            scan_height: height,
            generation: 0,
            created_at: 0,
            last_access: 0,
        }
    }

    #[test]
    fn lanes_group_by_height_and_prioritise_the_tip() {
        let accounts =
            vec![account(1, 100), account(2, 9_000), account(3, 150), account(4, 1_000_000), account(5, 999_990)];
        let lanes = make_lanes(accounts, 1_000);
        let heights: Vec<Vec<u64>> = lanes.iter().map(|l| l.iter().map(|a| a.scan_height).collect()).collect();
        assert_eq!(heights, vec![vec![100, 150], vec![9_000], vec![999_990, 1_000_000]]);

        let picked = pick_lanes(lanes, 2);
        assert_eq!(picked[0][0].scan_height, 999_990, "tip lane first");
        assert_eq!(picked[1][0].scan_height, 100, "then the furthest behind");
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn lease_names_are_per_shard() {
        assert_eq!(lease_name(None), "scanner-0-of-1");
        assert_eq!(lease_name(Some((2, 8))), "scanner-2-of-8");
    }
}
