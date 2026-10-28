# Operations

## Topologies

**Single machine** (development, small private servers):

```bash
beldex-lws serve --role all --database sqlite:/var/lib/beldex-lws/lws.db ...
```

**Production:**

| Process | Count | Command |
| --- | --- | --- |
| Migrate (once per upgrade) | 1 | `beldex-lws migrate` |
| Wallet API | as many as load needs | `serve --role api --no-migrate --database-read <replica>` |
| Scanner | 1 per shard, plus optional standbys | `serve --role scanner --no-migrate --scan-shard i/N` |
| PostgreSQL | primary, plus replicas | |
| beldexd | 2 or more | listed in `--daemon` |

- API replicas share nothing but the database, so put any TLS load balancer in
  front of them. Run each with `--trust-forwarded-for` only if the balancer
  sets `X-Forwarded-For`.
- Size scanner shards with `make bench`: shards needed ≈ (active accounts ×
  transactions per block) ÷ (measured checks per second × 30 s), plus headroom.
- Two processes with the same `--scan-shard` are safe: one holds the lease and
  the other waits as a hot standby, taking over within the lease TTL (30 s).
- Changing the shard count is safe at any time. Shards are leased by name
  (`scanner-i-of-N`) and every commit is checked per account.

## Several beldexd

List every daemon in `--daemon` (comma-separated, primary first):

```bash
--daemon http://10.0.0.1:19091,http://10.0.0.2:19091,http://10.0.0.3:19091
```

- **Parallel downloads.** Each scan lane keeps `--fetch-window` block requests
  in flight (by default twice the number of daemons, at most 8), each started
  at a different daemon. Batches are applied strictly in height order and
  checked block by block (each must extend the previous hash), so a daemon on
  a different fork can only stall a batch, never corrupt one. This helps most
  with remote daemons and full mainnet blocks, where downloading dominates.
- **Failover.** A daemon that errors or times out is benched with exponential
  backoff (up to 60 s) and requests move to the next. A benched daemon is still
  tried as a last resort, so one recovering is noticed straight away.
- **Lagging daemons.** Each daemon's chain height is tracked. A daemon more
  than 5 blocks behind the best is tried after the others. A request for
  blocks it does not have yet falls through to one that has them, instead of
  being read as "nothing new".
- `/health` on the admin listener shows each daemon's state, height and last
  error.

## Configuration

Every option has an environment variable (`LWS_*`, see `beldex-lws serve
--help`), and can also come from a TOML file passed with `--config` (see
[deploy/lws.example.toml](../deploy/lws.example.toml)). Precedence is command
line, then environment, then file.

Keep secrets out of command lines, which other users can see in `ps`: pass
the database URL in `LWS_DATABASE_URL`, and keys and tokens as files.

Hardware defaults:

- async workers and scan threads both default to every core
  (`--worker-threads`, `--scan-threads`);
- database pools default to 4 connections per core, between 8 and 64
  (`--db-max-connections`). Keep `api replicas × pool size` under PostgreSQL's
  `max_connections`, or use PgBouncer in transaction mode.

## Keys

```bash
beldex-lws keygen /etc/beldex-lws/seal.key      # encrypts view keys at rest
beldex-lws keygen /etc/beldex-lws/admin.token   # bearer token for the admin API
```

Both files are created with mode 0600, and the server refuses files other
users can read.

- **Back up the sealing key separately from the database.** Without it, the
  stored view keys cannot be opened, although every wallet can still log in
  again and be rescanned.
- **Rotate** the sealing key without downtime:
  1. `keygen` a new key.
  2. Restart every process with `--sealing-key-file new.key
     --sealing-key-id 2 --sealing-key-old 1:/etc/beldex-lws/seal.key`.
  3. Run `beldex-lws admin reseal` with the same options.
  4. Drop `--sealing-key-old`.
- `--allow-plaintext-keys` exists for development only.

## TLS

Use `--tls-cert`/`--tls-key` (PEM) to terminate TLS in the server, or put a TLS
proxy in front and pass `--confirm-external-bind`. Plain HTTP on a public
address is refused otherwise. Wallets send view keys.

Keep the admin listener (`--admin-listen`) on loopback or a private network.
Binding it to `0.0.0.0` is refused.

## Account policy

- `--account-creation auto` (the default): `/login` creates accounts, limited
  by `--creations-per-hour` per client.
- `manual`: `/login` files a request. Approve it with `beldex-lws admin
  accept-requests create <address>` or `POST /accept_requests`.
- `disabled`: `/login` returns 501 for unknown accounts.
- Import requests (rescan from genesis) need approval unless
  `--auto-accept-imports` is set. Leave that off on public servers: a
  full-history rescan is expensive, and anyone holding a view key can ask for one.

## Administration

`beldex-lws admin ...` works directly on the database, and is safe while the
server runs:

```bash
beldex-lws admin list-accounts
beldex-lws admin list-requests
beldex-lws admin accept-requests import <address>...
beldex-lws admin set-status inactive <address>...    # stop scanning, keep serving
beldex-lws admin rescan 1200000 <address>...         # the API keeps serving meanwhile
beldex-lws admin rollback 1234000 --confirm          # every account, as after a reorg
beldex-lws admin export-accounts accounts.json       # contains view keys; 0600
beldex-lws admin import-accounts accounts.json
beldex-lws admin reseal
```

The admin HTTP API offers the same operations (see [API.md](API.md)).

## Backups and recovery

- **PostgreSQL:** use the usual tools (`pg_basebackup` with WAL archiving for
  point-in-time recovery, or streaming replicas). Everything in the database
  can be rebuilt from the chain, so the essentials are the accounts table and
  the sealing key.
- **SQLite:** `sqlite3 lws.db ".backup /backups/lws-$(date +%F).db"` is safe
  while the server runs (the database is in WAL mode).
- **Full rebuild:** `export-accounts` on the old database, `import-accounts`
  into a new one. Every account rescans from its start height.

## Migrating from the LMDB server

**Drop-in.** Install `beldex-lws` and point the old binary name at it:

```bash
ln -sf /opt/beldex-lws/bin/beldex-lws /usr/local/bin/beldex-lws-daemon
```

Invoked as `beldex-lws-daemon`, it implies `serve` and accepts the LMDB
server's command line and `key=value` config file unchanged. On first start
in the old `--db-path` it:

1. creates its own SQLite database (`beldex-lws.sqlite`) and a sealing key
   (`beldex-lws-seal.key`, mode 0600) in that directory;
2. imports every account (status, flags, start height) and pending request
   from the LMDB database there, read-only;
3. rebuilds each account from the chain, from its start height.

Wallets and the old admin tooling keep working: the wallet API, admin routes,
admin-account authorisation and response shapes are the same.

| LMDB server option | Here |
| --- | --- |
| `--rest-server http(s)://host:port` | `--listen` (+ TLS) |
| `--rest-ssl-key`, `--rest-ssl-certificate` | `--tls-key`, `--tls-cert` |
| `--admin-rest-server` | `--admin-listen` |
| `--access-control-origin` | `--cors-origin` |
| `--daemon`, `--daemon-backup` | `--daemon` (one list) |
| `--scan-threads`, `--confirm-external-bind`, `--network`, `--config-file` | same |
| `--rest-max-response-bytes`, `--rest-cache-bytes` | `--max-response-mb`, `--response-cache-mb` |
| `--log-level 0-4` | same, or names (`info`, `debug`, ...) |
| `--db-path` | SQLite and sealing key in that directory, plus the one-time import |
| `--db-map-size`, `--db-max-readers`, `--create-queue-max`, `--daemon-spread`, `--sub`, `--rest-threads`, `--exchange-rate-interval` | accepted, no effect (noted at startup) |

Differences to know about:

- The admin API listens on `127.0.0.1:8444` by default. The LMDB server had
  none unless `--admin-rest-server` was given. If the wallet API itself uses
  port 8444, the admin API stays off until `--admin-listen` is passed.
- A wrong view key is answered 403 and a malformed address 400, as the
  lightwallet spec says. The LMDB server answered 500 for both.
- `/switch_db` answers 501: rescans run in place, so there is nothing to
  switch to.

**Into PostgreSQL or another machine.** Import explicitly:

```bash
beldex-lws admin import-lmdb /var/lib/beldex-lws     # reads data.mdb read-only
# or, from an export made with the old tool:
beldex-lws admin import-accounts accounts.json
```

Read from a stopped server or from a `beldex-lws-backup` copy. The old server's
LMDB lock format differs from this reader's, so it reads without locks.

Either way, run the new server alongside the old one until it reaches the tip
(`beldex_lws_min_scan_height` in the metrics), then switch the load balancer.

## Monitoring

The admin listener serves:

- `GET /metrics`: Prometheus. Alert on:
  - `beldex_lws_daemon_height - beldex_lws_min_scan_height` growing (scanner falling behind);
  - `scan_errors_total` rising;
  - `http_server_errors_total` and `http_shed_total` rising (add API replicas);
  - `http_rate_limited_total` (abuse, or limits set too tight).
- `GET /ready`: 200 once the database and at least one daemon are reachable.
  Use it for load-balancer health checks.
- `GET /health`: details on the database, daemon endpoints and scanner.

Logs go to stderr. Use `--log-format json` for log shippers. View keys and
tokens are never logged.

## Troubleshooting

- **`refusing to scan: daemon is on 'testnet' ...`**: `--network` and the
  daemon disagree. This is deliberate.
- **`database belongs to testnet, but the server is configured for mainnet`**:
  the database is pinned to the network it was first used with. Use a
  separate database for each network.
- **`unsupported tx version`**: the chain has a transaction format this kernel
  build does not know. Update the `external/beldex` submodule to a node release
  that includes it, then rebuild. The scanner stops rather than skip data.
- **`not JSON ... is the daemon URL right?`**: `--daemon` points at something
  that is not `beldexd` RPC.
- **Many 503s with `http_shed_total` rising**: the replica is at
  `--max-in-flight`. Add replicas, or raise the limit if CPU and the database
  have headroom.
