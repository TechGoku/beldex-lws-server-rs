# API

## Wallet API

Wallets call it with `POST /<method>` and a JSON body. The request and response
shapes are those of the LMDB server on `lws-implementation`
(`src/lws/lightwallet_rest.md` there), so existing wallets work unchanged.

Conventions:

- `uint64-string` values (amounts) are decimal strings. Numbers are accepted on
  input too, and anything out of range is rejected.
- Binary values are lowercase hex. Timestamps look like `2023-11-14T22:13:20Z`.
- Unknown request fields are ignored.

| Method | Auth | Notes |
| --- | --- | --- |
| `login` | view key | `create_account`, `generated_locally`. Returns `new_address`, `generated_locally`, `start_height` |
| `get_address_info` | view key | Balance scalars, candidate `spent_outputs`, per-token `tokens`. `min_height`, `max_count` |
| `get_address_txs` | view key | History, one entry per tx, `token_legs` for privacy tokens. `min_height`, `max_count` |
| `get_unspent_outs` | view key | Spendable outputs with `rct` and `spend_key_images`. `token_id`, `all_tokens`, `min_height`, `max_count` |
| `get_random_outs` | none | Decoys: gamma distribution for ringct, triangular for legacy amounts, token bucket per `token_ids` |
| `import_request` | view key | Asks for a rescan from genesis |
| `submit_raw_tx` | none | `tx` hex; `fee: "5"` relays as a flash transaction |
| `daemon_status` | none | Daemon height and connectivity; `state: "unavailable"` when unreachable |
| `get_token_balances` | view key | Per-token totals with the token's descriptor |
| `get_token_info` | none | One token descriptor |
| `get_token_list` | none | `offset`, `count` (at most 1000) |
| `health` | none | `{"status":"ok"}` when the database answers |

### Incremental fetch

`min_height` is an inclusive cursor. `max_count` caps a page without ever
splitting a block, and `next_min_height` (0 on the last page) is the cursor
for the next page. Seed the cursor about 10 blocks below the last scanned
height so a small reorg cannot drop anything. De-duplicate on `hash` for
transactions, `key_image` for spends and `public_key` for outputs.

`total_sent` and `spent_outputs` are candidates. The server holds only the view
key, so a ring that merely uses one of your outputs as a decoy also appears.
Wallets confirm spends with their own key images.

### Status codes

| Code | Meaning |
| --- | --- |
| 200 | OK |
| 400 | Malformed request, bad address (the LMDB server answered 500), unknown token |
| 403 | Unknown, hidden or unapproved account; wrong view key (the LMDB server answered 500); `get_unspent_outs` amount above the balance |
| 404 | No such method |
| 405 | Not POST |
| 429 | Rate limited (`Retry-After`) |
| 501 | Account creation disabled |
| 503 | Overloaded, timed out, daemon unavailable, or response over `--max-response-mb` |

## Admin API

This runs on `--admin-listen`. Routes, bodies and responses are those of the
LMDB server's admin API, so its scripts and tools keep working. Protected
routes accept either form of authorisation:

- `Authorization: Bearer <contents of --admin-token-file>`, or
- the LMDB server's body form: `{"auth": "<view key of an admin account>",
  "params": {...}}`. The account must be flagged admin
  (`admin add-account` / `/add_account` with `"admin": true`) and not inactive.

A missing or wrong bearer token gives 401; a wrong `auth` key gives 403.

| Route | `params` | Response |
| --- | --- | --- |
| `POST /list_accounts` | none | `{active, inactive, hidden}`: arrays of `{address, scan_height, access_time, ...}` |
| `POST /list_requests` | none | `{create, import}`: arrays of `{address, start_height, ...}` |
| `POST /add_account` | `address`, `key` (`start_height?`, `admin?`) | `{updated: [address]}` |
| `POST /accept_requests` | `type` (`create`/`import`), `addresses` | `{updated: [...]}` |
| `POST /reject_requests` | `type`, `addresses` | `{updated: [...]}` |
| `POST /modify_account_status` | `status` (`active`/`inactive`/`hidden`), `addresses` | `{updated: [...]}` |
| `POST /rescan` | `height`, `addresses` | `{updated: [...]}` |
| `POST /validate` | `spend_public_hex`, `view_public_hex`, `view_key_hex` | `{address}` or `{error: {field, details}}` |
| `POST /switch_db` | | 501: not needed; rescans run in place |
| `GET /metrics` | | Prometheus text, no auth |
| `GET /health` | | Details, no auth |
| `GET /ready` | | 200 or 503, no auth |

`updated` lists the addresses that were changed. Unknown addresses are left out.
