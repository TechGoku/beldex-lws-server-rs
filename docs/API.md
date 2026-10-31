# API

## Wallet API

Wallets call it with `POST /<method>` and a JSON body. The request and response
shapes are those of the LMDB server on `lws-implementation`
(`src/lws/lightwallet_rest.md` there), so existing wallets work unchanged.

Conventions:

- `uint64-string` values (amounts) are decimal strings, in atomic units. Numbers
  are accepted on input too, and anything out of range is rejected.
- Binary values are lowercase hex. Timestamps look like `2023-11-14T22:13:20Z`.
- Unknown request fields are ignored.
- `address` and `view_key` are the account's standard address and secret view
  key. Calls marked "view key" need both.

| Method | Auth | Request limit | Notes |
| --- | --- | --- | --- |
| [`login`](#login) | view key | 2 KiB | Opens or creates an account |
| [`get_address_info`](#get_address_info) | view key | 2 KiB | Balance totals, candidate spends, per-token totals |
| [`get_address_txs`](#get_address_txs) | view key | 2 KiB | History, one entry per transaction |
| [`get_unspent_outs`](#get_unspent_outs) | view key | 2 KiB | Spendable outputs and current fees |
| [`get_random_outs`](#get_random_outs) | none | 16 KiB | Decoys: gamma distribution for ringct, triangular for legacy amounts, token bucket per `token_ids` |
| [`submit_raw_tx`](#submit_raw_tx) | none | 256 KiB | Sends a signed transaction |
| [`import_request`](#import_request) | view key | 2 KiB | Asks for a rescan from genesis |
| [`daemon_status`](#daemon_status) | none | | Daemon height and connectivity |
| [`get_token_balances`](#get_token_balances) | view key | 8 KiB | Per-token totals with each token's descriptor |
| [`get_token_info`](#get_token_info) | none | 1 KiB | One token descriptor |
| [`get_token_list`](#get_token_list) | none | 1 KiB | Token ids, a page at a time |
| `health` | none | | `{"status":"ok"}` when the database answers. Also accepts GET |

Long hex values are shortened with `...` in the examples below.

### login

```json
{"address": "...", "view_key": "...", "create_account": true, "generated_locally": false}
```

`create_account` and `generated_locally` default to `false`. Without
`create_account`, an unknown account gets 403.

```json
{"new_address": true, "generated_locally": false, "start_height": 124943}
```

A new account starts at the current chain height. Use
[`import_request`](#import_request) for funds received before that. With
`--account-creation manual`, the account waits for approval. With `disabled`,
creating one gets 501. Creations are limited per client (`--creations-per-hour`).

### get_address_info

```json
{"address": "...", "view_key": "...", "min_height": 0, "max_count": 100}
```

`min_height` and `max_count` are optional. See
[Incremental fetch](#incremental-fetch).

```json
{
  "locked_funds": "0",
  "total_received": "487453875130",
  "total_sent": "250000000000",
  "scanned_height": 124943,
  "scanned_block_height": 124943,
  "start_height": 0,
  "transaction_height": 124943,
  "blockchain_height": 124943,
  "next_min_height": 0,
  "spent_outputs": [
    {"amount": "250000000000", "key_image": "ef34...", "tx_pub_key": "714d...", "out_index": 1, "mixin": 9}
  ],
  "tokens": []
}
```

Each `tokens` entry has `token_id`, `total_received`, `total_sent` and
`locked_funds`.

### get_address_txs

The request is the same as `get_address_info`.

```json
{
  "total_received": "487453875130",
  "locked_funds": "0",
  "scanned_height": 124943,
  "scanned_block_height": 124943,
  "start_height": 0,
  "transaction_height": 124943,
  "blockchain_height": 124943,
  "next_min_height": 0,
  "transactions": [
    {
      "id": 0,
      "hash": "20dd...",
      "timestamp": "2026-10-29T10:49:22Z",
      "total_received": "250000000000",
      "total_sent": "0",
      "unlock_time": 0,
      "height": 124883,
      "payment_id": "0000000000000000",
      "coinbase": false,
      "mempool": false,
      "mixin": 9,
      "spent_outputs": []
    }
  ]
}
```

`payment_id` is left out when there is none. Privacy token movements are in
`token_legs`, left out when empty: `token_id`, `received`, `sent`, and
`spent_outputs` when there are any.

### get_unspent_outs

```json
{"address": "...", "view_key": "...", "amount": "1000000000", "mixin": 9, "use_dust": true, "dust_threshold": "0"}
```

Only `amount` is required, as well as the credentials. An `amount` above the
balance gets 403. BDX outputs are always returned. `token_id` adds one token's
outputs and `all_tokens: true` adds every token's. `min_height` and `max_count`
are also optional.

```json
{
  "fee_per_byte": 6666,
  "fee_per_output": 10000000,
  "flash_fee_per_byte": 19998,
  "flash_fee_per_output": 30000000,
  "flash_fee_fixed": 0,
  "quantization_mask": 10,
  "fork_version": 22,
  "amount": "487453875130",
  "next_min_height": 0,
  "blockchain_height": 124943,
  "outputs": [
    {
      "amount": "250000000000",
      "public_key": "916a...",
      "index": 1,
      "global_index": 125548,
      "tx_id": 125548,
      "tx_hash": "20dd...",
      "tx_prefix_hash": "b1ba...",
      "tx_pub_key": "714d...",
      "timestamp": "2026-10-29T10:49:22Z",
      "height": 124883,
      "spend_key_images": ["ef34..."],
      "rct": "d549..."
    }
  ]
}
```

Token outputs also carry `token_id`, `blinded_token_id`, `amount_commitment` and
`encrypted_amount`.

### get_random_outs

```json
{"count": 10, "amounts": ["0"], "token_ids": []}
```

`count` is the number of decoys per ring, at most 50. `amounts` has one entry
per ring, at most 200. `token_ids`, when given, lines up with `amounts`, and a
valid id draws that ring's decoys from the token bucket.

```json
{
  "amount_outs": [
    {"amount": "0", "outputs": [{"global_index": "112968", "public_key": "9b2e...", "rct": "84c7..."}]}
  ]
}
```

Token decoys also carry `blinded_token_id`.

### submit_raw_tx

```json
{"tx": "<signed transaction hex>", "fee": "5"}
```

`fee` is optional, and `"5"` relays the transaction as a flash transaction. Any
other value relays it normally.

On success the status is 200 and the body is exactly:

```json
{"status": "OK"}
```

**Any other status means the transaction was not accepted.** A failure has this
JSON body:

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

| Field | Meaning |
| --- | --- |
| `status` | Always `FAILED` |
| `code` | What went wrong, from the table below. Use this in code |
| `Error`, `error` | The same message for the user, under the two names existing clients read |
| `may_be_sent` | `true` if the transaction could still reach the network, so sending it again could pay twice. Check the wallet's history first. `false` if it was not accepted |
| `reason` | `beldexd`'s own text, at most 500 characters, for logs and support. Only when `beldexd` gave a verdict |
| `reason_codes` | `beldexd`'s list of causes, in no fixed order. Only when `beldexd` sent one |
| `flash_status` | `beldexd`'s flash result: `1` rejected, `3` timed out. Only for flash transactions |

| `code` | HTTP | `may_be_sent` | Cause |
| --- | --- | --- | --- |
| `invalid_request` | 400 | false | Malformed JSON, or `tx` is not hex |
| `double_spend` | 500 | false | An input is already spent |
| `overspend` | 500 | false | It spends more than its inputs hold |
| `fee_too_low` | 500 | false | The fee is too low |
| `mixin` | 500 | false | Wrong number of decoys |
| `invalid_input`, `invalid_output` | 500 | false | A bad input or output |
| `too_few_outputs`, `too_big` | 500 | false | Too few outputs, or too large |
| `invalid_version`, `invalid_type` | 500 | false | Not valid at this hard fork. The wallet may need an update |
| `mnode_locked`, `blacklisted` | 500 | false | The funds are staked, or in a master node penalty period |
| `altchain` | 500 | false | It spends outputs on another chain |
| `failed` | 500 | false | It failed verification. See `reason` |
| `rejected` | 500 | false | `beldexd` rejected it with no known code |
| `flash_rejected` | 500 | false | The flash quorum refused it |
| `flash_timeout` | 500 | true | The flash quorum did not answer in time |
| `not_relayed` | 500 | true | `beldexd` did not relay it. This is also the answer for a transaction it already has, in the pool or on the chain |
| `daemon_busy` | 500 | false | `beldexd` is still syncing. Try again shortly |
| `bad_daemon_response` | 500 | true | `beldexd`'s answer could not be understood |
| `daemon_error` | 503 | false | `beldexd` refused the call |
| `daemon_unavailable` | 503 | true | No `beldexd` answered, or the call timed out |

A rejection is never a 200, because wallets take any 200 as sent without reading
the body. The rate limiter, the load shedder and the request size limit answer
before this handler runs, with the plain-text bodies listed under
[Status codes](#status-codes). Those are failures too, and `may_be_sent` does not
apply to a 429, shed 503 or 413, since they never reach `beldexd`. A request
timeout also gets a plain-text 503, and that one may have reached `beldexd`, so
treat a 503 without a JSON body as "check first".

Existing clients keep working:

- The `@bdxi` web and mobile clients show `Error`, where they used to show a
  generic error such as `Unknown 500 error`.
- The browser extension checks the HTTP status only and still reports `HTTP 500`.
- Everything that relied on the old 500, 503 and 400 statuses sees the same
  status as before.

### import_request

```json
{"address": "...", "view_key": "..."}
```

```json
{"import_fee": "0", "status": "Accepted, waiting for approval", "new_request": true, "request_fulfilled": false}
```

| `status` | Meaning |
| --- | --- |
| `Accepted, waiting for approval` | Request recorded just now |
| `Waiting for Approval` | Already requested and not yet approved |
| `Approved` | Rescan approved (`request_fulfilled: true`). The account now starts from genesis |

Approve with `beldex-lws admin accept-requests import <address>`, the dashboard,
or `--auto-accept-imports`.

### daemon_status

The body can be `{}`.

```json
{"outgoing_connections_count": 8, "incoming_connections_count": 2, "height": 1001, "target_height": 1001, "network": "test", "state": "ok"}
```

`state` is `ok`, `synchronizing`, `no_connections` or `unavailable`. `network` is
`main`, `test`, `stage` or `fake`. When `state` is `unavailable`, the counts and
heights are 0 and `network` has no meaning.

### get_token_balances

```json
{"address": "...", "view_key": "...", "token_ids": []}
```

`token_ids` is optional, at most 100, and adds tokens to the reply even if the
account has none of them.

```json
{"tokens": [], "scanned_height": 124943, "blockchain_height": 124943}
```

Each `tokens` entry has `token_id`, `status` (`confirmed`, `not_found` or
`unknown`), `total_received`, `total_sent`, `locked_funds` and `unlocked_balance`.
A `confirmed` token also has the fields of `get_token_info`.

### get_token_info

```json
{"token_id": "<64 hex characters>"}
```

```json
{
  "token_id": "1ff6...",
  "ticker": "TNA",
  "full_name": "TNA e2e token",
  "owner": "10a5...",
  "meta_info": "",
  "current_supply": "100000000",
  "total_max_supply": "500000000",
  "decimal_point": 2
}
```

An unknown token gets 400.

### get_token_list

```json
{"offset": 0, "count": 100}
```

Both are optional. `count` defaults to 100, at most 1000.

```json
{"token_ids": ["1ff6..."], "total_count": 15}
```

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

Apart from `submit_raw_tx` failures, an error's body is plain text, such as
`Forbidden`. Clients should go by the status code.

| Code | Meaning |
| --- | --- |
| 200 | OK |
| 400 | Malformed request, bad address (the LMDB server answered 500), unknown token |
| 403 | Unknown, hidden or unapproved account; wrong view key (the LMDB server answered 500); `get_unspent_outs` amount above the balance |
| 404 | No such method |
| 405 | Not POST |
| 413 | Request larger than the method's limit |
| 429 | Rate limited (`Retry-After`) |
| 500 | Internal error, or `submit_raw_tx` rejected by `beldexd` |
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

### JSON API (`/api/v1`)

The dashboard's API, for scripts and other tools too. Same authorisation as
above (in practice the bearer token). Errors are `{"error": "..."}` with the
usual status codes. View keys are never returned, and every change is
recorded in the audit log with the caller (`token`, `admin-account <prefix>`
or `cli:<user>`) and address.

| Route | Input | Response |
| --- | --- | --- |
| `GET /api/v1/overview` | | Status, chain height, daemons, account and request counts, scanner lag, this process's metrics (histograms with p50/p95/p99 and raw buckets), every instance of the deployment and the scanner leases |
| `GET /api/v1/accounts` | `q` (address prefix), `status`, `behind=true`, `limit` (≤ 500), `before` | `{accounts: [{id, address, status, admin, scan_height, behind, start_height, created_at, last_access}], next_before}`, newest first |
| `GET /api/v1/accounts/{address}` | | One account, with its output and spend counts |
| `POST /api/v1/accounts` | `{address, view_key, start_height?, admin?}` | 201 `{created: true, ...}`, or 200 `{created: false}` when it exists |
| `POST /api/v1/accounts/status` | `{status, addresses}` | `{updated: [...]}` |
| `POST /api/v1/accounts/rescan` | `{height, addresses}` | `{updated: [...]}` |
| `GET /api/v1/requests` | | `{requests: [{kind, address, start_height, created_at}]}` |
| `POST /api/v1/requests/accept`, `/reject` | `{kind: "create" \| "import", addresses}` | `{updated: [...]}` |
| `GET /api/v1/audit` | `limit`, `before` | `{entries: [{id, at, actor, remote, action, detail}], next_before}`, newest first |
| `GET /api/v1/config` | | This process's effective settings, without secrets |

Address lists are limited to 10,000 per call. Pages use keyset pagination:
pass `next_before` back as `before` until it is `null`.

