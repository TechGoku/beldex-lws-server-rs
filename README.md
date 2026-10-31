# beldex-lws

A light wallet server for Beldex: it scans the chain with wallets' view keys
and serves the lightwallet REST API, so wallets can show balances and history
and build transactions without holding the blockchain.

It is a from-scratch replacement for the LMDB-based server on the
`lws-implementation` branch of the Beldex node. Wallets need no changes: the
REST API is the same, field for field, including privacy tokens (HF22) and
`min_height` incremental fetch. What changes is everything underneath. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how and why.

- **Rust service, C++ kernel.** Networking, storage, scheduling and HTTP are
  memory-safe Rust. A small C++ kernel, linked against beldex core at a pinned
  commit, parses blocks and transactions and decodes amounts and tokens with the
  node's own code.
- **PostgreSQL or SQLite.** PostgreSQL in production; SQLite for development
  and single-machine installs. No map size, reader slots, single-writer lock,
  shadow databases or custom backup tool.
- **Scales out.** Stateless API replicas behind a load balancer (reads can go to
  a replica); scanner shards that coordinate through database leases, with hot
  standbys.
- **Uses the hardware.** Scanning runs on every core. A fast prefilter screens
  accounts about 4.7x faster than beldex core's maths would, and the C++ kernel
  confirms whatever it flags. The next block downloads while the current one is
  being scanned.
- **Easy to run and watch.** `install.sh` (or `docker compose up`) sets
  everything up; `setup --web` does the configuration in a browser. A
  built-in dashboard shows health, live charts, every account and every
  process of the deployment, with an audit log of every change. A Grafana
  dashboard and Prometheus alerts ship in `deploy/monitoring/`.
- **Secure by default.**
  - View keys are encrypted at rest.
  - Credentials are checked in constant time.
  - Clients are rate-limited, with a separate limit on account creation.
  - Excess load is shed with an immediate 503.
  - The admin API runs on its own listener and needs a bearer token.
  - Plain HTTP on a public address is refused.

## Getting started

This takes you from a clean machine to a running server with a wallet connected.

You need a `beldexd` on the network you want to serve, reachable over RPC and
synced. By default it answers on port 19091 on mainnet, 29091 on testnet and
39091 on devnet.

### 1. Install the build tools

On Ubuntu 24.04:

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake ninja-build pkg-config git curl \
  libboost-all-dev libsodium-dev libssl-dev libsqlite3-dev libzmq3-dev libcurl4-openssl-dev
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
```

Other systems need CMake 3.16+, Ninja, a C++17 compiler, Boost, libsodium,
OpenSSL, SQLite, ZeroMQ, libcurl and Rust 1.80+.

### 2. Build

```bash
git clone --recursive https://github.com/TechGoku/beldex-lws-server-rs.git
cd beldex-lws-server-rs
make release
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/beldex-lws" ~/.local/bin/
```

The first build also compiles beldex core, so it takes a while. The last two
lines let you type `beldex-lws` from any folder. `~/.local/bin` must be on your
`PATH`, and the link stops working if you move this folder.

### 3. Set up

Answer the questions in your browser:

```bash
beldex-lws setup --web
```

Or in the terminal:

```bash
beldex-lws setup
```

Both ask the same three things:

| Question | Answer |
| --- | --- |
| Which `beldexd`? | It looks on this machine and takes the network from the daemon it finds. If yours uses another port, start with `--daemon http://127.0.0.1:PORT` |
| Which database? | SQLite for one machine, or a `postgres://` URL (PostgreSQL 13+) for production |
| How do wallets reach the server? | This machine only, through a TLS proxy such as Caddy, or with TLS certificates served by the server itself |

Setup writes `~/.beldex-lws/lws.toml` and creates two keys next to it. It
prints the exact commands for the next step.

To skip the questions, pass the answers:

```bash
beldex-lws setup --yes --network test --daemon http://127.0.0.1:29091
```

> **Back up `beldex-lws-seal.key` with the database.** Without that key, the
> stored view keys cannot be read.

### 4. Check and start

```bash
beldex-lws doctor --config ~/.beldex-lws/lws.toml
beldex-lws serve --config ~/.beldex-lws/lws.toml
```

`doctor` checks the keys, the database, `beldexd`, TLS and the ports, and says
what to fix. A `no peers` warning is normal on a private testnet. `serve` runs
until you press Ctrl-C.

To test it, run this in a second terminal:

```bash
curl -X POST http://127.0.0.1:8443/daemon_status -d '{}'
```

The reply shows the height of `beldexd` and the network:

```json
{"height":124944,"incoming_connections_count":0,"network":"test","outgoing_connections_count":0,"state":"no_connections","target_height":0}
```

`state` is `ok` once `beldexd` has peers.

### 5. Connect a wallet

Set the wallet's server address to `http://127.0.0.1:8443`. A remote wallet
needs HTTPS (see [Public server](#public-server)). A browser wallet served from
another address also needs `--cors-origin https://your-wallet.example`.

The wallet registers itself when it logs in, and **a new account starts at the
current chain height**. A wallet with older funds won't show them until it is
rescanned:

1. The wallet asks for a rescan through `/import_request`.
2. You approve it:

   ```bash
   beldex-lws admin --config ~/.beldex-lws/lws.toml accept-requests import <address>
   ```

To approve these automatically, start the server with `--auto-accept-imports`.
You can also add a wallet yourself with `beldex-lws admin add-account`, which
takes a `--start-height`.

### 6. Open the dashboard

Go to <http://127.0.0.1:8444/> and sign in with the token from
`~/.beldex-lws/beldex-lws-admin.token`. The dashboard shows health, live charts,
accounts, requests and the audit log.

## Other ways to run it

### As a systemd service

Use this for a server that should keep running and start at boot. After
`make release`:

```bash
sudo ./deploy/install.sh         # add --web to answer in a browser
```

This installs to `/opt/beldex-lws`, runs the same setup, starts the service and
runs `doctor`. To manage it:

```bash
systemctl status beldex-lws
sudo systemctl restart beldex-lws
journalctl -u beldex-lws -f
```

To upgrade, keeping the config and data:

```bash
git pull --recurse-submodules
make release
sudo ./deploy/install.sh --upgrade
```

### With Docker

This runs the server with PostgreSQL. `beldexd` runs separately, so set its
address and the network in `deploy/.env` first:

```bash
cp deploy/.env.example deploy/.env
docker compose -f deploy/docker-compose.yml up -d
```

To read the dashboard token:

```bash
docker compose -f deploy/docker-compose.yml exec api cat /var/lib/beldex-lws/keys/beldex-lws-admin.token
```

### Public server

The server refuses plain HTTP on a public address, so choose one of these:

- **TLS proxy (recommended).** Run `beldex-lws setup --behind-proxy`. Setup
  prints a working Caddy config. With Docker, set `LWS_DOMAIN` in `deploy/.env`
  and add `--profile public` to `docker compose`, which starts Caddy for you.
- **Certificates served directly.** Run
  `beldex-lws setup --tls-cert fullchain.pem --tls-key privkey.pem`.

Keep the admin port, 8444, private. For PostgreSQL, several API replicas or
scanner shards, see [docs/OPERATIONS.md](docs/OPERATIONS.md).

### Replacing the LMDB server

Point `beldex-lws-daemon` at this binary and keep the existing command line,
config file and `--db-path`. On first start it imports the old accounts and
rebuilds them from the chain. To install it as a new service instead and bring
the accounts along, stop the old server and run
`sudo ./deploy/install.sh --import-lmdb <old --db-path>`. See
[docs/OPERATIONS.md](docs/OPERATIONS.md#migrating-from-the-lmdb-server).

## Where things are

| | Run by hand | systemd service | Docker |
| --- | --- | --- | --- |
| Config | `~/.beldex-lws/lws.toml` | `/etc/beldex-lws/lws.toml` | `deploy/.env` |
| Keys and token | `~/.beldex-lws/` | `/var/lib/beldex-lws/` | `keys` volume |
| Database | `~/.beldex-lws/beldex-lws.sqlite` | as chosen in setup | `db` volume (PostgreSQL) |
| Logs | the terminal | `journalctl -u beldex-lws` | `docker compose logs` |

The server listens on two ports:

| Address | For | Auth |
| --- | --- | --- |
| `127.0.0.1:8443` | Wallet API | the wallet's view key, on account calls |
| `127.0.0.1:8444` | Dashboard, admin API, `/metrics` | admin token |

## Everyday commands

Add `--config <file>` to each of these, using the path above. For the systemd
service, also put `sudo -u beldex-lws` in front.

| Command | What it does |
| --- | --- |
| `beldex-lws doctor` | Checks the installation and says what is wrong |
| `beldex-lws admin list-accounts` | Lists accounts with their status and scan height |
| `beldex-lws admin list-requests` | Lists requests waiting for approval |
| `beldex-lws admin accept-requests import <address>` | Approves a rescan |
| `beldex-lws admin rescan <height> <address>` | Rescans from a height while the server keeps serving |
| `beldex-lws admin set-status inactive <address>` | Stops scanning an account |

`beldex-lws --help` lists everything, and every option can go in the config file
or an `LWS_*` environment variable.

## API

Wallets call `POST /<method>` on port 8443 with a JSON body. These are the same
calls, fields and meanings as the LMDB server, so existing wallets need no
changes.

| Method | Auth | What it does |
| --- | --- | --- |
| `login` | view key | Opens or creates an account |
| `get_address_info` | view key | Balance totals and candidate spends |
| `get_address_txs` | view key | Transaction history |
| `get_unspent_outs` | view key | Spendable outputs and the current fees |
| `get_random_outs` | none | Decoys for building a transaction |
| `submit_raw_tx` | none | Sends a signed transaction |
| `import_request` | view key | Asks for a rescan from the start of the chain |
| `daemon_status` | none | The state of `beldexd` |
| `get_token_balances` | view key | Balance of each privacy token |
| `get_token_info` | none | One token's details |
| `get_token_list` | none | All token ids |
| `health` | none | Whether the database answers |

For example:

```bash
curl -X POST http://127.0.0.1:8443/get_address_info \
  -d '{"address": "<address>", "view_key": "<secret view key hex>"}'
```

A successful `submit_raw_tx` returns `{"status":"OK"}`. A failed one returns a
non-200 status and says why:

```json
{
  "status": "FAILED",
  "code": "double_spend",
  "Error": "Transaction rejected: these funds are already spent. Refresh the wallet and try again.",
  "error": "Transaction rejected: these funds are already spent. Refresh the wallet and try again.",
  "may_be_sent": false,
  "reason": "Verification failed, connection should be dropped, TX should NOT be relayed, Double spend TX",
  "reason_codes": ["not_relayed", "double_spend", "failed"]
}
```

[docs/API.md](docs/API.md) has every field, example and status code, plus the
admin API.

## Troubleshooting

| What you see | What to do |
| --- | --- |
| `beldex-lws: command not found` | Run `make release`, then link it as in step 2, or use `./target/release/beldex-lws` |
| `No beldexd answered` | Start `beldexd`, or pass `--daemon http://HOST:PORT` |
| `refusing to scan: daemon is on 'testnet' ...` | The configured network and `beldexd` disagree. Correct `network` in the config file |
| Address already in use | Another server is on 8443 or 8444. Stop it, or change `--listen` and `--admin-listen` |
| A wallet is missing older funds | New accounts start at the current height. Approve a rescan as in step 5 |
| `403` on every wallet call | The account is unknown or hidden, or the view key is wrong |
| A send fails | The reply's `code` and the server log's `daemon rejected submitted transaction` line give the reason |

[docs/OPERATIONS.md](docs/OPERATIONS.md#troubleshooting) covers more cases.

## Repository layout

| Path | What |
| --- | --- |
| `kernel/` | C++ scan kernel: C ABI (`include/lws_kernel.h`), self-test, chain fixture generator |
| `external/beldex` | beldex core, pinned submodule |
| `crates/lws-kernel` | Rust bindings, plus the fast ownership prefilter |
| `crates/lws-core` | Domain types and metrics |
| `crates/lws-store` | PostgreSQL / SQLite storage, migrations, view-key sealing |
| `crates/lws-daemon` | `beldexd` client with failover and the epee binary codec |
| `crates/lws-scanner` | Chain scanner |
| `crates/lws-api` | Wallet REST API, admin API and the dashboard (`assets/`) |
| `crates/beldex-lws` | The binary: `serve`, `setup`, `doctor`, `migrate`, `admin`, `keygen` |
| `deploy/` | installer, systemd unit, Dockerfile, compose file, example config, `monitoring/` (Prometheus, alerts, Grafana) |

## Tests

```bash
make test                                    # kernel self-test + all Rust tests on SQLite
LWS_TEST_POSTGRES_URL=postgres://user@host/postgres make test   # and on PostgreSQL
make bench                                   # scan throughput on this machine
```

The end-to-end test generates real consensus-format blocks with beldex core
(`kernel/tests/make_chain.cpp`). A mock `beldexd` serves them over
`get_blocks.bin`, and the real scanner and API run against that. The test then
switches the mock to a competing fork and checks that the server rolls back.

## Documentation

- [Architecture](docs/ARCHITECTURE.md): components, invariants, and how this differs from the LMDB server
- [Operations](docs/OPERATIONS.md): deployment, scaling, keys, backups, monitoring, migration
- [API](docs/API.md): the wallet and admin endpoints
