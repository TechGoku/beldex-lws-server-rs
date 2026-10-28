/*
 * Beldex LWS scan kernel - C ABI.
 *
 * The kernel is the only part of the light wallet server that understands the
 * Beldex consensus formats and elliptic-curve maths. It is deliberately small:
 * it parses block and transaction blobs with beldex core's own code, finds the
 * outputs that belong to a set of view keys, and performs the handful of key
 * operations the REST API needs. Everything else (networking, storage,
 * scheduling, HTTP) lives in the Rust service.
 *
 * Memory: every object the kernel returns is owned by the kernel and must be
 * released with the matching lws_*_free function. Inputs are only borrowed for
 * the duration of a call.
 *
 * Threads: a parsed batch is immutable, so lws_scan may be called on the same
 * batch from many threads at once (each with its own slice of accounts).
 */
#ifndef LWS_KERNEL_H
#define LWS_KERNEL_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32)
#define LWS_API __declspec(dllexport)
#else
#define LWS_API __attribute__((visibility("default")))
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define LWS_KERNEL_ABI_VERSION 3u

/* Status codes. Zero is success; everything else leaves outputs untouched. */
#define LWS_OK 0
#define LWS_ERR_ARGUMENT 1
#define LWS_ERR_BLOCK_PARSE 2
#define LWS_ERR_TX_PARSE 3
#define LWS_ERR_UNSUPPORTED_TX 4
#define LWS_ERR_INDICES 5
#define LWS_ERR_CRYPTO 6
#define LWS_ERR_ADDRESS 7
#define LWS_ERR_INTERNAL 8

#define LWS_NET_MAINNET 0
#define LWS_NET_TESTNET 1
#define LWS_NET_DEVNET 2

#define LWS_INPUT_KEY 1   /* txin_to_key: native BDX (amount may be 0 for ringct) */
#define LWS_INPUT_TOKEN 2 /* txin_zy_input: HF22 privacy token */

#define LWS_OUTPUT_KEY 1   /* txout_to_key */
#define LWS_OUTPUT_TOKEN 2 /* tx_out_zyphora */

#define LWS_FLAG_COINBASE 0x01
#define LWS_FLAG_RINGCT 0x02
#define LWS_FLAG_TOKEN 0x04

typedef struct lws_key { uint8_t data[32]; } lws_key;

typedef struct lws_blob {
  const uint8_t* data;
  size_t len;
} lws_blob;

/* One entry of a get_blocks.bin response, as decoded by the caller. */
typedef struct lws_block_entry {
  lws_blob block;
  const lws_blob* txs; /* in block.tx_hashes order */
  size_t tx_count;
  /* Global output indices, flattened: miner tx first, then each tx in order.
     index_counts[i] says how many indices belong to tx i (tx_count + 1 entries). */
  const uint64_t* indices;
  const uint32_t* index_counts;
  size_t index_count_len;
} lws_block_entry;

typedef struct lws_block {
  uint64_t height;
  uint64_t timestamp;
  lws_key hash;
  lws_key prev_hash;
  uint32_t tx_offset; /* into lws_batch.txs; the miner tx comes first */
  uint32_t tx_count;
} lws_block;

typedef struct lws_tx {
  lws_key hash;
  lws_key prefix_hash;
  lws_key tx_pub_key;
  lws_key locked_key_image; /* first key image proof, for stakes */
  uint64_t unlock_time;     /* legacy tx-wide value */
  uint32_t input_offset;
  uint32_t input_count;
  uint32_t output_offset;
  uint32_t output_count;
  uint32_t mixin; /* ring size of the last ring input, minus one */
  uint16_t version;
  uint16_t type;
  uint8_t has_tx_pub_key;
  uint8_t has_locked_key_image;
  uint8_t is_coinbase;
  uint8_t long_payment_id_len; /* 0 or 32: unencrypted payment id */
  uint8_t long_payment_id[32];
  uint8_t has_short_payment_id; /* encrypted 8-byte id present in extra */
  uint8_t short_payment_id[8];  /* still encrypted */
} lws_tx;

typedef struct lws_input {
  lws_key key_image;
  uint64_t amount; /* 0 for ringct and token inputs */
  uint32_t ring_offset; /* into lws_batch.ring_members (absolute global indices) */
  uint32_t ring_count;
  uint8_t kind;
} lws_input;

typedef struct lws_output {
  lws_key pub_key; /* one-time key (stealth_address for token outputs) */
  lws_key commitment;        /* ringct outPk mask, or token amount commitment */
  lws_key blinded_token_id;  /* token outputs only */
  uint64_t amount;           /* plaintext on-chain amount (0 when hidden) */
  uint64_t global_index;
  uint64_t unlock_time;      /* per-output unlock time */
  uint64_t encrypted_amount; /* token outputs only */
  uint32_t index;            /* position in vout */
  int32_t native_index;      /* position among native outputs, -1 for tokens */
  uint8_t kind;
} lws_output;

typedef struct lws_batch lws_batch;

typedef struct lws_batch_view {
  const lws_block* blocks;
  size_t block_count;
  const lws_tx* txs;
  size_t tx_count;
  const lws_input* inputs;
  size_t input_count;
  const lws_output* outputs;
  size_t output_count;
  const uint64_t* ring_members;
  size_t ring_member_count;
} lws_batch_view;

typedef struct lws_account {
  lws_key view_secret;
  lws_key spend_public;
  uint64_t scan_height; /* blocks at or below this height are skipped */
} lws_account;

typedef struct lws_match {
  uint32_t account; /* index into the accounts passed to lws_scan */
  uint32_t tx;      /* index into lws_batch_view.txs */
  uint32_t output;  /* index into lws_batch_view.outputs */
  uint8_t flags;
  uint8_t payment_id_len; /* 0, 8 (decrypted short id) or 32 (long id) */
  uint8_t payment_id[32];
  uint8_t has_rct; /* rct below is set (coinbase and ringct outputs) */
  uint64_t amount; /* decoded amount, in BDX or in the token's units */
  lws_key mask;    /* ringct mask, identity for coinbase */
  lws_key token_id; /* zero for native outputs */
  uint8_t rct[96]; /* the API's rct blob, as lws_output_rct computes it */
} lws_match;

LWS_API uint32_t lws_kernel_abi_version(void);

/* sizeof() of every struct above, in declaration order: lws_key, lws_blob,
   lws_block_entry, lws_block, lws_tx, lws_input, lws_output, lws_batch_view,
   lws_account, lws_match. Callers compare against their own layout at startup.
   Returns the number of sizes the kernel knows; writes min(n, that) of them. */
LWS_API size_t lws_struct_sizes(size_t* out, size_t n);

/* Writes a NUL-terminated description of the last error on this thread. */
LWS_API size_t lws_last_error(char* out, size_t out_len);

LWS_API int lws_parse_blocks(const lws_block_entry* entries, size_t entry_count, int pruned, lws_batch** out);
LWS_API void lws_batch_get(const lws_batch* batch, lws_batch_view* out);
LWS_API void lws_batch_free(lws_batch* batch);

LWS_API int lws_scan(const lws_batch* batch, const lws_account* accounts, size_t account_count,
             lws_match** out, size_t* out_count);
LWS_API void lws_matches_free(lws_match* matches);

LWS_API int lws_address_decode(const char* address, size_t len, uint8_t network,
                       lws_key* view_public, lws_key* spend_public);
/* Writes up to out_len-1 characters plus a NUL; returns the full length. */
LWS_API size_t lws_address_encode(const lws_key* view_public, const lws_key* spend_public,
                          uint8_t network, char* out, size_t out_len);

/* 1 if view_secret is a canonical scalar whose public key is view_public. */
LWS_API int lws_view_key_matches(const lws_key* view_secret, const lws_key* view_public);
LWS_API int lws_secret_to_public(const lws_key* secret, lws_key* public_out);
/* Fresh random keypair from the kernel's CSPRNG. */
LWS_API void lws_generate_keypair(lws_key* secret_out, lws_key* public_out);

/* The 96-byte `rct` blob the lightwallet API returns for an owned output:
   commitment || encrypted mask || encrypted amount (ecdh v1 encoding). */
LWS_API int lws_output_rct(const lws_key* tx_pub_key, const lws_key* view_secret, uint32_t output_index,
                   uint64_t amount, const lws_key* mask, uint8_t out[96]);

/* The one-time output key a sender derives for (tx_pub_key, view, index,
   spend_public), using beldex core's own derivation. Lets other
   implementations of the ownership check be verified against the kernel. */
LWS_API int lws_derive_output_key(const lws_key* tx_secret, const lws_key* view_public, uint32_t output_index,
                                  const lws_key* spend_public, lws_key* tx_pub_out, lws_key* output_key_out);

/* commit(0, identity) style helper: Pedersen commitment of `amount` with `mask`. */
LWS_API int lws_commit(uint64_t amount, const lws_key* mask, lws_key* out);

#ifdef __cplusplus
}
#endif

#endif
