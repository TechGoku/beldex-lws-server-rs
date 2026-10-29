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

## Replacing the LMDB server

Point `beldex-lws-daemon` at this binary and keep the existing command line,
config file and `--db-path`. On first start it imports the old accounts and
rebuilds them from the chain. Wallets and admin tools keep working. Or
install it fresh as a service and bring the accounts along with
`sudo ./deploy/install.sh --import-lmdb <old --db-path>`. See
[docs/OPERATIONS.md](docs/OPERATIONS.md#migrating-from-the-lmdb-server).

## Quick start

```bash
git clone --recursive <this repo> && cd beldex-lws-scalable
make release                 # builds beldex core, the kernel and the server
sudo ./deploy/install.sh     # installs, configures and starts the systemd service
```

The installer finds the local beldexd (and with it the network), asks how
wallets will connect, creates the keys and starts the service. Then it checks
everything with `beldex-lws doctor`. Add `--web` to answer the questions in a
browser. With Docker instead:

```bash
docker compose -f deploy/docker-compose.yml up -d
```

To try it without installing anything, run `./target/release/beldex-lws serve
--network test`. It needs no config file: it keeps its database and keys in
`~/.beldex-lws` and finds beldexd at its default port. To call it as
`beldex-lws` without installing, link it onto your PATH:
`ln -s "$PWD/target/release/beldex-lws" ~/.local/bin/`.

Wallets connect to `127.0.0.1:8443`. The dashboard and admin API are on
`http://127.0.0.1:8444/`; sign in with the token in `beldex-lws-admin.token`. Wallets register themselves through `/login`. For a public
server, a TLS proxy, PostgreSQL, several API replicas or sharded scanners, see
[docs/OPERATIONS.md](docs/OPERATIONS.md).

## Requirements

- A synced `beldexd` on the same network, reachable over RPC.
- Build: CMake 3.16+, a C++17 compiler, Boost, libsodium, OpenSSL and Rust 1.80+.
- PostgreSQL 13+ for production.

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
