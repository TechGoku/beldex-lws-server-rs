# Operations

## Installing

Every way of running it needs a `beldexd` on the same network, reachable over
RPC (synced, for mainnet).

**Linux server with systemd (recommended).** From a release archive or a
source tree after `make release`. Each `v*` tag publishes an archive on the
[releases page](https://github.com/TechGoku/beldex-lws-server-rs/releases),
built on Ubuntu 24.04 for any x86-64 CPU (`make dist` builds the same locally).
It needs only the runtime libraries:

```bash
sudo apt-get install -y libsodium23 libzmq5 libboost-serialization1.83.0 libboost-program-options1.83.0
```

Then, from the unpacked archive or the source tree:

```bash
sudo ./install.sh
```

It installs to `/opt/beldex-lws`, creates the `beldex-lws` system user, runs
`beldex-lws setup` (below) to write `/etc/beldex-lws/lws.toml`, starts the
`beldex-lws` service and runs `doctor`. Options:

| | |
| --- | --- |
| `--yes --network test --daemon URL ...` | no questions; any `setup` option is passed on |
| `--import-lmdb DIR` | also import the accounts of an LMDB server's `--db-path` (stop that server first) |
| `--no-start` | install and enable, but do not start |
| `--upgrade` | replace the binaries and restart; config and data stay |
| `--uninstall` | remove the service and binaries; config and data stay |
| `--destdir DIR` | stage the files only, for building packages |

`sudo make install ARGS="--yes"` does the same from a source tree.

**Docker.** No files to prepare: keys are created in a volume on first start.
Compose builds the image, or set `LWS_IMAGE=ghcr.io/techgoku/beldex-lws:latest`
in `deploy/.env` and run `docker compose -f deploy/docker-compose.yml pull` to
use the one each release publishes.

```bash
cp deploy/.env.example deploy/.env      # network, beldexd address, domain
docker compose -f deploy/docker-compose.yml up -d
docker compose -f deploy/docker-compose.yml --profile public up -d   # + Caddy with HTTPS for LWS_DOMAIN
```

The dashboard is then on `http://127.0.0.1:8444/` of the host.

**By hand.** `beldex-lws serve` runs with no configuration at all: it keeps a
SQLite database, the sealing key and the admin token in its data directory
(`~/.beldex-lws`, `/var/lib/beldex-lws` as root, or systemd's
`StateDirectory`), talks to beldexd at the network's default RPC port on this
machine (19091 main, 29091 test, 39091 dev), and subscribes to that beldexd's
block announcements when it finds its socket in the usual place
(`~/.beldex/[testnet/]beldexd.sock` or `/var/lib/beldex/...`).

```bash
beldex-lws serve --network test        # that is all
```

### setup

`beldex-lws setup` writes a commented config file and creates the keys. On a
terminal it asks three questions, each with a default: which beldexd (it
probes the default ports and takes the network from the daemon it finds),
which database (SQLite, or a `postgres://` URL it then tests), and how wallets
reach the server (this machine only, a TLS proxy such as Caddy on this
machine, or TLS certificates served directly). `--yes` takes the defaults and
flags without asking. It refuses to overwrite an existing config unless
`--force` is given.

### setup in a browser

`beldex-lws setup --web` (or `sudo ./install.sh --web`) asks the same
questions in a browser, with buttons that test the daemon, the database and
the TLS files before anything is written. It listens on `127.0.0.1:8445` only
and prints a one-time link; on a remote server, forward the port first
(`ssh -L 8445:127.0.0.1:8445 server`) and open the link locally. It stops once
the configuration is written.

### doctor

`beldex-lws doctor` takes the same options and config file as `serve` and
checks, one line each:

- the scan kernel, the config and the key files (permissions and format);
- the database, and that the sealing key opens its view keys;
- every beldexd (reachable, right network, synced, has peers, not lagging);
- the notification socket;
- the TLS certificate;
- the listen ports, and a server already running on them (with its scan progress).

It exits non-zero when something would stop the server from working, so it
also fits scripts and health checks.

## Topologies

**Single machine** (development, small and medium servers): one process,
SQLite, as `install.sh` sets up.

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
  `--idle-after-days` lowers the number of active accounts (see
  [Account policy](#account-policy)).
- Release archives and the Docker image are built for any x86-64 CPU. A
  source build with `make release NATIVE=1` is compiled for the build
  machine's CPU and scans a few percent faster, but may crash on another
  machine: build it where it runs, and never copy it elsewhere.
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
- **Instant new blocks.** With `--notify` (alias `--sub`, as on the LMDB
  server), the scanner subscribes to beldexd's OxenMQ block announcements and
  starts on a new block at once instead of at the next poll:

  ```bash
  --notify ipc:///var/lib/beldex/beldexd.sock      # beldexd's default local socket
  --notify tcp://10.0.0.2:20202                    # beldexd --lmq-public tcp://...
  ```

  Measured on testnet: 0-25 ms from block to scan, against the 5 s default
  poll. The subscription is renewed every 10 s; a daemon restart is noticed on
  the next renewal and the scanner catches up straight away. Polling stays on
  as the fallback. `beldex_lws_block_notifications_connected` and
  `_total` are in the metrics.
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

`setup`, the installer and a first `serve` with SQLite create both keys in the
data directory (`beldex-lws-seal.key`, `beldex-lws-admin.token`). To make them
yourself:

```bash
beldex-lws keygen /etc/beldex-lws/seal.key      # encrypts view keys at rest
beldex-lws keygen /etc/beldex-lws/admin.token   # bearer token for the admin API
beldex-lws keygen --keep FILE                   # only if FILE does not exist yet
```

Both files are created with mode 0600, and the server refuses files other
users can read. With PostgreSQL the sealing key is never created implicitly:
every server sharing the database needs the same key, so create it once and
copy it. Each server checks at startup that its key opens the database's view
keys, and refuses to start otherwise.

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
- `--idle-after-days N` (default 0, off): accounts no wallet has opened for
  N days are not scanned. Scan cost grows with the number of scanned
  accounts, and on a public server most accounts are never opened again. When
  a wallet opens an idle account, the scanner picks it up within a few
  seconds and catches it up from where it stopped; until then the wallet sees
  `scanned_height` behind `blockchain_height`, as for any account that is
  syncing. Accounts added by an operator count as opened when added.

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
beldex-lws admin backup /backups/lws-$(date +%F).db   # consistent copy; 0600
```

The admin HTTP API offers the same operations (see [API.md](API.md)).

## Backups and recovery

Back up the sealing key once, separately from the database: without it the
view keys in a backup cannot be read, and anyone holding both can read them.

- **`beldex-lws admin backup FILE`** works with either database while the
  server runs, and writes a new file readable only by its owner. With SQLite
  it is a ready-to-use copy of the database; with PostgreSQL it runs
  `pg_dump` (install the PostgreSQL client tools) and writes a custom-format
  archive. A daily copy, kept for two weeks, from the `beldex-lws` user's
  crontab:

  ```cron
  15 3 * * * /opt/beldex-lws/bin/beldex-lws admin --config /etc/beldex-lws/lws.toml backup /var/backups/beldex-lws/lws-$(date +\%F).db && find /var/backups/beldex-lws -name 'lws-*' -mtime +14 -delete
  ```

- **Restoring SQLite:** stop the server, replace the database file
  (`beldex-lws.sqlite` in the data directory) with the copy, start it. The
  scanner continues from the copy's heights.
- **Restoring PostgreSQL:** `pg_restore --clean --if-exists -d <database> FILE`
  with the server stopped.
- **PostgreSQL at scale:** `pg_basebackup` with WAL archiving gives
  point-in-time recovery. Everything in the database can be rebuilt from the
  chain, so the essentials are the accounts table and the sealing key.
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
| `--sub ipc://.../beldexd.sock` | `--notify` (instant new-block wake-ups) |
| `--db-map-size`, `--db-max-readers`, `--create-queue-max`, `--daemon-spread`, `--rest-threads`, `--exchange-rate-interval` | accepted, no effect (noted at startup) |

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

## Dashboard

The admin listener serves a web dashboard at `/` (default
`http://127.0.0.1:8444/`). Sign in with the admin token
(`beldex-lws-admin.token` in the data directory). From another machine,
forward the port: `ssh -L 8444:127.0.0.1:8444 server`.

- **Overview:** health, accounts, scanner lag, chain height, requests per
  second, p50/p95 latency, error and shed rates, live charts, every beldexd
  with its height and errors, scanning and database details.
- **Accounts:** search by address prefix, filter by status or "behind",
  page through millions of accounts, see one in detail, add, activate,
  deactivate, hide, or rescan (one account or a selection).
- **Requests:** approve or reject pending account creations and imports.
- **Cluster:** every process of the deployment (API replicas and scanners)
  with its version, uptime, last report and key numbers, and which scanner
  holds each shard. Processes report every 10 seconds through the database.
- **Audit log:** every change made through the dashboard, the admin API or
  `beldex-lws admin`, with who made it and from where. Entries are kept for
  `--audit-retention-days` (365; 0 keeps everything).
- **Settings:** the effective configuration, without secrets.

The page loads nothing from other sites and runs under a strict
Content-Security-Policy. The token stays in the tab's memory (or its session
storage, with "stay signed in"), is sent as a bearer token, and no cookies
are used. Values from the server are only ever inserted as text.

Scanner-only processes (`--role scanner`) serve `/metrics`, `/health` and
`/ready` on their admin listener, so every process can be scraped.

## Monitoring

`deploy/monitoring/` has a Prometheus scrape example, alert rules
(`alerts.yml`: process down, scanner behind or stalled, scan errors, 5xx
ratio, p95 latency, load shedding, lost block notifications, restart loops)
and a Grafana dashboard (`grafana-dashboard.json`; import it and pick the
Prometheus data source).

The admin listener serves:

- `GET /metrics`: Prometheus, including latency histograms
  (`http_request_duration_seconds`, `scan_batch_duration_seconds`,
  `daemon_fetch_duration_seconds`), `build_info` and
  `process_start_time_seconds`. Alert on:
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

Start with `beldex-lws doctor --config /etc/beldex-lws/lws.toml` (as the
service user): it names most of the problems below directly.

- **`the sealing key cannot open this database's view keys`**: the server was
  given a different key from the one the database was written with: another
  machine's, or a regenerated one. Restore the right key file. While rotating,
  pass the old key with `--sealing-key-old`.
- **Notifications `off` although beldexd runs here**: the service user cannot
  open beldexd's socket (permissions, or the systemd unit's `ProtectHome=` when
  beldexd lives under a home directory). Give the user access, or use
  `beldexd --lmq-public tcp://127.0.0.1:PORT` with `--notify tcp://127.0.0.1:PORT`.
  Unix socket paths are limited to 108 bytes.
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
- **A wallet says a transaction was rejected**: the reply's `code` says why (see
  [API.md](API.md#submit_raw_tx)), and the log has a `daemon rejected submitted
  transaction` line with `beldexd`'s full reason.
- **Many 503s with `http_shed_total` rising**: the replica is at
  `--max-in-flight`. Add replicas, or raise the limit if CPU and the database
  have headroom.
