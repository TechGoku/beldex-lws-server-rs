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

Every command below can be pasted as it is, whether you are a normal user or
root. They were tested on Ubuntu 24.04. Replace only the values in `<angle
brackets>`.

### 1. Check `beldexd`

You need a synced `beldexd` on the same machine, or one you can reach over RPC.
This should print its height:

```bash
curl -s http://127.0.0.1:19091/get_height
```

The RPC port is 19091 on mainnet, 29091 on testnet and 39091 on devnet.

### 2. Install the build tools

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake ninja-build pkg-config git curl libboost-all-dev libsodium-dev libssl-dev libsqlite3-dev libzmq3-dev libcurl4-openssl-dev
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
```

If you are root and `sudo` is not installed, leave `sudo` out. Other systems need
CMake 3.16+, Ninja, a C++17 compiler, Boost, libsodium, OpenSSL, SQLite, ZeroMQ,
libcurl and Rust 1.80+.

### 3. Build

```bash
cd ~
git clone --recursive https://github.com/TechGoku/beldex-lws-server-rs.git
cd beldex-lws-server-rs
make release
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/beldex-lws" ~/.local/bin/
export PATH="$HOME/.local/bin:$PATH"
beldex-lws --version
```

The first build also compiles beldex core, so it takes several minutes and a
fair amount of memory. The last line should print the version.

`make release NATIVE=1` instead compiles for this machine's CPU and scans a few
percent faster. Use it only for a binary that stays on the machine that built
it: on an older CPU it crashes.

### 4. Set up

This asks three questions. Press Enter to accept a default:

```bash
beldex-lws setup --data-dir ~/.beldex-lws --out ~/.beldex-lws/lws.toml
```

| Question | Answer |
| --- | --- |
| Which `beldexd`? | It looks on this machine and takes the network from the daemon it finds. If yours uses another port, type its address, such as `http://127.0.0.1:28081` |
| Which database? | SQLite for one machine, or a `postgres://` URL (PostgreSQL 13+) for production |
| How do wallets reach the server? | This machine only, through a TLS proxy such as Caddy, or with TLS certificates served by the server itself |

To skip the questions, use this instead. This example is for mainnet. On testnet,
use `--network test` and port 29091:

```bash
beldex-lws setup --yes --network main --daemon http://127.0.0.1:19091 --data-dir ~/.beldex-lws --out ~/.beldex-lws/lws.toml
```

To answer in a browser, use this instead:

```bash
beldex-lws setup --web --data-dir ~/.beldex-lws --out ~/.beldex-lws/lws.toml
```

The browser page only listens on the server itself. On a remote server, first
connect with this command from your own computer, then open the link setup prints:

```bash
ssh -L 8445:127.0.0.1:8445 <user>@<your-server>
```

Setup writes `~/.beldex-lws/lws.toml` and creates two keys next to it. To change
your answers later, run setup again with `--force` added, which keeps the keys and
the database.

> **Back up `~/.beldex-lws/beldex-lws-seal.key` with the database.** Without that
> key, the stored view keys cannot be read.

### 5. Check and start

```bash
beldex-lws doctor --config ~/.beldex-lws/lws.toml
beldex-lws serve --config ~/.beldex-lws/lws.toml
```

`doctor` checks the keys, the database, `beldexd`, TLS and the ports, and says
what to fix. A `no peers` warning is normal on a private testnet. `serve` then
runs until you press Ctrl-C, so leave that terminal open.

To test it, run this in a second terminal on the same machine:

```bash
curl -X POST http://127.0.0.1:8443/daemon_status -d '{}'
```

The reply shows the height of `beldexd` and the network:

```json
{"height":124944,"incoming_connections_count":0,"network":"test","outgoing_connections_count":0,"state":"no_connections","target_height":0}
```

`state` is `ok` once `beldexd` has peers.

### 6. Connect a wallet

On the same machine, set the wallet's server address to `http://127.0.0.1:8443`.
A remote wallet needs HTTPS (see [Public server](#public-server)). A browser
wallet served from another address also needs `--cors-origin
https://your-wallet.example` on `serve`.

The wallet registers itself when it logs in, and **a new account starts at the
current chain height**. A wallet with older funds won't show them until it is
rescanned. The wallet asks for that through `/import_request`, and you approve it:

```bash
beldex-lws admin --config ~/.beldex-lws/lws.toml accept-requests import <address>
```

To approve these automatically, add `--auto-accept-imports` to `serve`. You can
also add a wallet yourself with `beldex-lws admin add-account`, which takes a
`--start-height`.

### 7. Open the dashboard

Print the sign-in token:

```bash
cat ~/.beldex-lws/beldex-lws-admin.token
```

Then open <http://127.0.0.1:8444/>. On a remote server, first connect with this
command from your own computer:

```bash
ssh -L 8444:127.0.0.1:8444 <user>@<your-server>
```

The dashboard shows health, live charts, accounts, requests and the audit log.

## Other ways to run it

### From a release, without building

Each release on the [releases page](https://github.com/TechGoku/beldex-lws-server-rs/releases)
has a ready-built archive for Ubuntu 24.04 on x86-64. It replaces steps 2 and 3
and installs the systemd service described below:

```bash
sudo apt-get update
sudo apt-get install -y curl libsodium23 libzmq5 libboost-serialization1.83.0 libboost-program-options1.83.0
cd ~
VERSION=0.1.0
curl -LO "https://github.com/TechGoku/beldex-lws-server-rs/releases/download/v$VERSION/beldex-lws-$VERSION-linux-x86_64.tar.gz"
tar xzf "beldex-lws-$VERSION-linux-x86_64.tar.gz"
cd "beldex-lws-$VERSION"
sudo ./install.sh
```

Set `VERSION` to the newest release. On other systems, build from source.

### As a systemd service

Use this for a server that should keep running and start at boot. Do steps 1 to
3 first, and stop any `serve` you started by hand:

```bash
cd ~/beldex-lws-server-rs
sudo ./deploy/install.sh
```

This installs to `/opt/beldex-lws`, asks the setup questions again, starts the
service and runs `doctor`. Add `--web` to answer in a browser. To manage it:

```bash
systemctl status beldex-lws
```

```bash
sudo systemctl restart beldex-lws
```

```bash
journalctl -u beldex-lws -f
```

To read the dashboard token:

```bash
sudo cat /var/lib/beldex-lws/beldex-lws-admin.token
```

To upgrade, keeping the config and data:

```bash
cd ~/beldex-lws-server-rs
git pull --recurse-submodules
make release
sudo ./deploy/install.sh --upgrade
```

### With Docker

This runs the server with PostgreSQL, using the repository from step 3 or a fresh
clone of it. `beldexd` runs separately:

```bash
cd ~/beldex-lws-server-rs
cp deploy/.env.example deploy/.env
```

Open `deploy/.env` and set the network and the address of `beldexd`. The first
start builds the image. To use the published image instead, uncomment
`LWS_IMAGE` in `deploy/.env` and run
`docker compose -f deploy/docker-compose.yml pull` first. Then start it:

```bash
docker compose -f deploy/docker-compose.yml up -d
```

To read the dashboard token:

```bash
docker compose -f deploy/docker-compose.yml exec api cat /var/lib/beldex-lws/keys/beldex-lws-admin.token
```

### Public server

The server refuses plain HTTP on a public address, so choose one of these.

**TLS proxy (recommended).** Setup prints a working Caddy config:

```bash
beldex-lws setup --behind-proxy --force --data-dir ~/.beldex-lws --out ~/.beldex-lws/lws.toml
```

With Docker, set `LWS_DOMAIN` in `deploy/.env` and start it with Caddy instead:

```bash
docker compose -f deploy/docker-compose.yml --profile public up -d
```

**Certificates served directly:**

```bash
beldex-lws setup --tls-cert <fullchain.pem> --tls-key <privkey.pem> --force --data-dir ~/.beldex-lws --out ~/.beldex-lws/lws.toml
```

Keep the admin port, 8444, private. For PostgreSQL, several API replicas or
scanner shards, see [docs/OPERATIONS.md](docs/OPERATIONS.md).

### Replacing the LMDB server

Point `beldex-lws-daemon` at this binary and keep the existing command line,
config file and `--db-path`. On first start it imports the old accounts and
rebuilds them from the chain. To install it as a new service instead and bring
the accounts along, stop the old server and run this from the repository:

```bash
sudo ./deploy/install.sh --import-lmdb <old --db-path>
```

See [docs/OPERATIONS.md](docs/OPERATIONS.md#migrating-from-the-lmdb-server).

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

These are for the setup in the steps above. For the systemd service, start each
with `sudo -u beldex-lws /opt/beldex-lws/bin/beldex-lws` and use
`--config /etc/beldex-lws/lws.toml`.

Check the installation and say what is wrong:

```bash
beldex-lws doctor --config ~/.beldex-lws/lws.toml
```

List accounts with their status and scan height:

```bash
beldex-lws admin --config ~/.beldex-lws/lws.toml list-accounts
```

List requests waiting for approval:

```bash
beldex-lws admin --config ~/.beldex-lws/lws.toml list-requests
```

Rescan an account from a height while the server keeps serving:

```bash
beldex-lws admin --config ~/.beldex-lws/lws.toml rescan <height> <address>
```

Stop scanning an account:

```bash
beldex-lws admin --config ~/.beldex-lws/lws.toml set-status inactive <address>
```

Back up the database while the server keeps running. The copy can only be read
by you:

```bash
mkdir -p ~/lws-backups
beldex-lws admin --config ~/.beldex-lws/lws.toml backup ~/lws-backups/lws-$(date +%F).db
```

The sealing key, `~/.beldex-lws/beldex-lws-seal.key`, is not in the backup. Copy it
somewhere safe once: without it the backup's view keys cannot be read.

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
| `beldex-lws: command not found` | Run `export PATH="$HOME/.local/bin:$PATH"`, or log in again. If that does not help, repeat step 3 |
| `No beldexd answered` | Start `beldexd`, or add `--daemon http://HOST:PORT` to setup |
| `creating /var/lib/beldex-lws` or `Permission denied` as root | Add `--data-dir ~/.beldex-lws --out ~/.beldex-lws/lws.toml` to setup, as in step 4 |
| `already exists; replace it explicitly` | Add `--force` to setup. It replaces the config and keeps the keys and database |
| `refusing to scan: daemon is on 'testnet' ...` | The configured network and `beldexd` disagree. Correct `network` in the config file |
| Address already in use | Another server is on 8443 or 8444. Stop it, or change `--listen` and `--admin-listen` |
| A wallet is missing older funds | New accounts start at the current height. Approve a rescan as in step 6 |
| `403` on every wallet call | The account is unknown or hidden, or the view key is wrong |
| A send fails | The reply's `code` and the server log's `daemon rejected submitted transaction` line give the reason |
| `rate limited requests in the last minute` in the log | Clients went over `rate-limit` (20 a second per IP). Normal for a load test from one machine. Raise `rate-limit` and `rate-burst` in the config, or set `rate-limit = 0` while testing |
| A wallet takes a while to show its balance after months away | With `idle-after-days` set, its account was parked. It catches up on its own within minutes of the wallet opening it |

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
