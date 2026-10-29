# Architecture

## Components

```
                        wallets (web, mobile, desktop)
                                     |
                              TLS load balancer
                 +-------------------+-------------------+
                 |                   |                   |
           beldex-lws           beldex-lws          beldex-lws        role=api, stateless,
           --role api           --role api          --role api        any number
                 |                   |                   |
                 +---------+---------+---------+---------+
                           |  reads (replica)  |  writes
                           v                   v
                 PostgreSQL replica  <---  PostgreSQL primary
                                               ^
                           +-------------------+-------------------+
                           |                                       |
                     beldex-lws                              beldex-lws         role=scanner,
                     --role scanner --scan-shard 0/2         --scan-shard 1/2   one lease per shard,
                           |                                       |            standbys allowed
                           +------------------+--------------------+
                                              v
                                   beldexd (one or more, failover)
```

A single process (`--role all`, SQLite or PostgreSQL) runs everything for
small installs.

### C++ scan kernel (`kernel/`)

The kernel is the only code that understands Beldex consensus formats. It
exposes a small C ABI (`kernel/include/lws_kernel.h`) for:

- parsing a run of `get_blocks.bin` entries into flat arrays of blocks, txs,
  inputs (with absolute ring members) and outputs (with global indices);
- scanning those for a set of view keys: ownership, ringct amount decoding,
  HF22 privacy-token (zyphora) decoding with commitment verification, and
  payment-id decryption;
- addresses, view-key checks, and the API's `rct` blob.

It links beldex core statically from a pinned submodule, so it parses exactly
what the node accepts. A transaction version it does not know is a hard
error: the scanner stops and alerts rather than skipping data.

The ABI is versioned. Rust checks the version and every struct size at
startup, so a mismatched library fails before doing any work.

### Fast prefilter (`crates/lws-kernel/src/fast.rs`)

The scanner's cost is (accounts × transactions) elliptic-curve checks. The
prefilter computes the same ownership test as beldex core with
curve25519-dalek. It builds one precomputed table per transaction public key
and shares it across all accounts, which turns the per-account variable-base
multiplication into a fixed-base one.

The prefilter only screens. It reports (transaction, account) pairs, and the
kernel verifies exactly those pairs (`lws_scan_pairs`), so a hit costs one
kernel check, not a rescan of the batch for that account. The kernel alone
decides ownership and decodes amounts. The prefilter's tests check it against
the kernel's own key derivation on random keys.

Work is split into (transaction, block of 64 accounts) items, so every core is
busy whether a batch holds many transactions and few accounts (catching up)
or one transaction and millions of accounts (at the tip).

Measured on the development machine (12 threads), per account-transaction check:

| Path | 1 thread | 12 threads |
| --- | --- | --- |
| Kernel only (beldex core ref10) | 8,500/s | 41,000/s |
| Fast prefilter, then kernel on hits | 40,000/s | 192,000/s |

`make bench` measures your hardware. To size a deployment, divide (active
accounts × transactions per block) by the measured rate, and compare that with
the 30-second block time. A single 12-thread machine keeps a few hundred
thousand accounts at the tip. Beyond that, add scanner shards: each shard
scans only `id % count == index`.

### Scanner (`crates/lws-scanner`)

- **Lanes.** Accounts are grouped by scan height. Each pass runs the tip lane,
  which holds nearly every account, plus the furthest-behind lanes in
  parallel, so a wallet importing from genesis never slows balance updates for
  everyone else. Lanes are regrouped every pass and merge as they catch up.
- **Parallel fetching.** Up to `--fetch-window` block requests are in flight per
  lane, rotated across every `--daemon`, while the current batch is scanned.
  The batch size is learned from the daemons; gaps are filled and overlaps
  dropped, and batches are applied in height order with hash-chain checks
  between them. Daemons that are behind the tip are tried last.
- **New blocks.** With `--notify`, an OxenMQ subscription to beldexd wakes the
  scanner the moment a block is added (one wake per announcement); polling is
  the fallback.
- **Reorgs.** Each block request carries a sparse list of our block hashes:
  ten dense, then exponentially further back, ending with genesis. Stock
  `beldexd` answers from the highest one still on its chain, so a fork is
  detected without extra requests. Rolling back is one transaction.
- **Spends.** Ring members are looked up in the database by global index, plus
  outputs found earlier in the same batch. The scanner holds no per-account
  state in memory, so its memory does not grow with the number of accounts.
- **Coordination.** Each shard is owned through a lease row with a TTL. A second
  process for the same shard waits as a hot standby and takes over when the
  holder stops renewing.

### Storage (`crates/lws-store`)

One `Store` type over PostgreSQL or SQLite. Every query is written once and
compiled against both backends, so they cannot drift apart.

The scanner and API rely on these invariants:

1. A scanner commit writes outputs, spends, block hashes and the new scan
   height in **one transaction**. A reader never sees a scan height whose rows
   are missing.
2. A commit is checked against the **chain epoch**. Every rollback bumps the
   epoch, so a batch fetched before a reorg cannot be written after it.
3. Each account has a **generation**, bumped by every rescan and rollback.
   Progress made against an older generation is discarded, never merged.
4. Stored block hashes are compared with the batch's. A disagreement means the
   stored branch is stale, and it is rolled back.
5. Scan heights only move backwards through a rollback or an operator rescan,
   both of which delete the rows above the new height in the same transaction.
6. Chain `u64` values are stored bit-cast into `BIGINT` and never summed in
   SQL. Totals are computed in Rust with overflow checks.

Migrations are embedded in the binary. Each one runs once inside a
transaction, is recorded with a checksum, and is serialised across processes.

### API (`crates/lws-api`)

The REST layer is stateless apart from caches. Each replica protects itself:

- body limits per endpoint;
- a request timeout;
- a concurrency cap that sheds excess requests with an immediate 503 instead
  of queueing them;
- a sharded token-bucket limiter per client (IPv6 bucketed by /64);
- a stricter hourly limit on account creation.

Expensive endpoints share a single-flight response cache keyed by account,
generation, scan height and chain tip. A wallet polling every few seconds then
costs one computation per block, not one per poll. Daemon data (stakes, fees,
fork version, output distribution, token descriptors) is cached with a TTL,
refreshed by a single caller, and served stale for up to two minutes if
`beldexd` is briefly unavailable.

## Verified against a live chain

On the private `testnet5` (124,876 blocks, fork 22, privacy tokens), compared
with wallet2 as ground truth and with the LMDB server:

| | LMDB server | beldex-lws |
| --- | --- | --- |
| Unlocked outputs found (wallets A, T) | 32/49, 139/142 | 49/49, 142/142 |
| Real spends among candidates (A, T) | 6/10, 83/113 | 10/10, 113/113 |
| `tx_prefix_hash` of token txs (46 txs) | 0 correct | 46 correct (checked with beldex core) |
| Token balances (wallets C, Ext, A) | same as beldex-lws | same as LMDB server |
| Full scan from genesis, 4 wallets | | 9.7 s (12 threads, PostgreSQL) |

## Security model

- **View keys** are sealed with XChaCha20-Poly1305 under a key kept outside the
  database. The account address is the associated data, so a sealed key moved
  to another row fails to open. Keys rotate with `admin reseal`. In memory, view
  keys are zeroed on drop and never printed.
- **Authentication.** A view key must derive the address's public view key
  (checked by the kernel), and must equal the stored key (compared in constant
  time). Hidden and unknown accounts get the same 403.
- **The admin API** runs on a separate listener (refused on `0.0.0.0` unless
  `--admin-any-interface` is given for a container) and needs
  `Authorization: Bearer`, compared in constant time.
- **Wrong sealing key.** A check value sealed with the current key is kept in
  the database; a server given another key refuses to start.
- **The dashboard** is static files compiled into the binary, served under a
  strict Content-Security-Policy (`default-src 'none'`, scripts and styles
  from itself only, no framing). It uses bearer tokens, not cookies, so it has
  no CSRF surface, and inserts server data as text only.
- **Audit trail.** Every administrative change (admin API, dashboard,
  `beldex-lws admin`) is stored with the actor, source address and details,
  never a view key, and logged under the `audit` target.
- **Browser setup** listens on loopback only, requires a one-time token
  carried in the URL fragment, checks the Host header against DNS rebinding,
  and exits after writing the configuration.
- **Transport.** TLS is built in (rustls). Plain HTTP on a non-loopback address
  is refused unless the operator confirms a TLS proxy sits in front.
- **Secret files** (sealing key, admin token) must be mode 0600. Exports are
  written 0600 and never overwrite an existing file.
- **Network binding.** The database is bound to a network on first use, and the
  scanner refuses a daemon on another network. A foreign chain would otherwise
  look like a deep reorg.
- **Untrusted daemon input.**
  - The epee decoder bounds every length by the bytes remaining and caps
    nesting depth.
  - The kernel refuses inconsistent output-index lists rather than
    misattributing outputs.
  - Token outputs are only stored after their commitment verifies.

## Compared with the LMDB server (`lws-implementation`)

| | LMDB server | beldex-lws |
| --- | --- | --- |
| Code | ~22k lines of C++ inside the node tree | Standalone; Rust service, small C++ kernel |
| Storage | LMDB: one writer, map size, reader slots | PostgreSQL or SQLite, row-level concurrency |
| Fixes it needed | `MDB_MAP_FULL`, orphaned reader slots, migrations re-running | Not applicable, or handled by the database |
| Block source | Custom JSON `get_blocks_fast` (node-side re-encoding) | Stock `get_blocks.bin`, parsed by beldex core |
| Reorg detection | Separate hash comparison | Built into the block request (split point) |
| Rescan / rollback | Outage, or shadow DB + rebuild tool + `/switch_db` | One transaction per account; the API keeps serving |
| Backups | Custom `beldex-lws-backup` | PostgreSQL tooling (PITR, replicas); SQLite online backup |
| Scaling | One process | API replicas plus scanner shards with leases |
| Scan cost | ref10 per account | Shared per-tx tables (about 4.7x), all cores |
| View keys at rest | Plaintext | Encrypted (XChaCha20-Poly1305) |
| `locked_funds` | Master-node list cached 10 s; requests fail once beldexd is down | Cached, single-flight, served stale for 2 min while beldexd is down |
| API wire format | Reference | Same |
| Setup | Manual flags and files | `install.sh`, `setup` (terminal or browser), `doctor` |
| Operations UI | None | Dashboard: health, charts, accounts, requests, cluster, audit log |
| Audit | None | Every admin change, with actor and source |
| Metrics | None | Prometheus with latency histograms; Grafana dashboard and alert rules |

Measured on the development machine (12 threads; the load generator, the
database and both servers on the same machine; 20,000 accounts polled in turn
with `get_address_info` and `get_address_txs`; runs vary by about 20%):

| | LMDB server | beldex-lws (PostgreSQL) |
| --- | --- | --- |
| Account-transaction checks per second, 12 threads | 33,000 | 184,000 |
| Wallet polls served per second, response cache on | 39,000 | 31,000–38,000 |
| Wallet polls served per second, response cache off | 32,000–41,000 | 16,000–20,000 |
| Machines that can share the work | 1 | API replicas and scanner shards on any number |

On a single machine the LMDB server answers a poll faster: it reads a
memory-mapped file in-process, where beldex-lws makes a query to PostgreSQL.
What beldex-lws adds is scan throughput per core and the ability to spread
both scanning and serving over many machines.
