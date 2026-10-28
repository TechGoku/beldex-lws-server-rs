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
rebuilds them from the chain. Wallets and admin tools keep working. See
[docs/OPERATIONS.md](docs/OPERATIONS.md#migrating-from-the-lmdb-server).

## Quick start (SQLite, testnet)

```bash
git clone --recursive <this repo> && cd beldex-lws-scalable
make release                      # builds beldex core, the kernel and the server

./target/release/beldex-lws keygen /etc/beldex-lws/seal.key
./target/release/beldex-lws keygen /etc/beldex-lws/admin.token

export LWS_DATABASE_URL=sqlite:/var/lib/beldex-lws/lws.db
export LWS_SEALING_KEY_FILE=/etc/beldex-lws/seal.key
./target/release/beldex-lws serve \
    --network test \
    --daemon http://127.0.0.1:29091 \
    --admin-token-file /etc/beldex-lws/admin.token
```

The wallet API listens on `127.0.0.1:8443` and the admin API on
`127.0.0.1:8444`. Wallets register themselves through `/login`.

For production (PostgreSQL, TLS, several API replicas, sharded scanners),
see [docs/OPERATIONS.md](docs/OPERATIONS.md).

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
| `crates/lws-api` | Wallet REST API and admin API |
| `crates/beldex-lws` | The binary: `serve`, `migrate`, `admin`, `keygen` |
| `deploy/` | systemd units, Dockerfile, compose file, example config |

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
