// Beldex LWS scan kernel. See include/lws_kernel.h for the contract.

#include "lws_kernel.h"

#include <algorithm>
#include <cstring>
#include <exception>
#include <limits>
#include <new>
#include <optional>
#include <string>
#include <string_view>
#include <variant>
#include <vector>

#include "crypto/crypto.h"
#include "crypto/hash.h"
#include "cryptonote_basic/cryptonote_basic.h"
#include "cryptonote_basic/cryptonote_basic_impl.h"
#include "cryptonote_basic/cryptonote_format_utils.h"
#include "cryptonote_basic/tx_extra.h"
#include "cryptonote_config.h"
#include "ringct/rctOps.h"
#include "ringct/rctTypes.h"

struct lws_batch
{
  std::vector<lws_block> blocks;
  std::vector<lws_tx> txs;
  std::vector<lws_input> inputs;
  std::vector<lws_output> outputs;
  std::vector<uint64_t> ring_members;
  std::vector<uint64_t> tx_height; // height of the block holding each tx
  // Parsed transactions are kept for scanning: rct ecdh data and token
  // ciphertexts are needed only for outputs that turn out to be owned.
  std::vector<cryptonote::transaction> parsed;
};

namespace
{
  thread_local std::string last_error;

  int fail(int code, std::string message)
  {
    last_error = std::move(message);
    return code;
  }

  template<typename T>
  void copy_key(lws_key& dest, const T& src) noexcept
  {
    static_assert(sizeof(T) == sizeof(dest.data), "key size mismatch");
    std::memcpy(dest.data, std::addressof(src), sizeof(dest.data));
  }

  template<typename T>
  T to_pod(const lws_key& src) noexcept
  {
    static_assert(sizeof(T) == sizeof(src.data), "key size mismatch");
    T out;
    std::memcpy(std::addressof(out), src.data, sizeof(out));
    return out;
  }

  std::string_view view(const lws_blob& blob) noexcept
  {
    return {reinterpret_cast<const char*>(blob.data), blob.len};
  }

  std::optional<cryptonote::network_type> to_network(uint8_t network) noexcept
  {
    switch (network)
    {
      case LWS_NET_MAINNET: return cryptonote::MAINNET;
      case LWS_NET_TESTNET: return cryptonote::TESTNET;
      case LWS_NET_DEVNET: return cryptonote::DEVNET;
      default: return std::nullopt;
    }
  }

  uint32_t checked_u32(std::size_t value)
  {
    if (std::numeric_limits<uint32_t>::max() < value)
      throw std::length_error{"batch exceeds 2^32 entries"};
    return static_cast<uint32_t>(value);
  }

  //! Fills `out` from tx extra. The FIRST tx public key is the one outputs are
  //! derived from; a miner tx can carry several.
  void read_extra(const cryptonote::transaction& tx, lws_tx& out)
  {
    std::vector<cryptonote::tx_extra_field> fields;
    cryptonote::parse_tx_extra(tx.extra, fields); // partial parses keep what was read

    cryptonote::tx_extra_pub_key pub{};
    if (cryptonote::find_tx_extra_field_by_type(fields, pub, 0))
    {
      copy_key(out.tx_pub_key, pub.pub_key);
      out.has_tx_pub_key = 1;
    }

    cryptonote::tx_extra_nonce nonce{};
    if (cryptonote::find_tx_extra_field_by_type(fields, nonce))
    {
      crypto::hash long_id{};
      crypto::hash8 short_id{};
      if (cryptonote::get_payment_id_from_tx_extra_nonce(nonce.nonce, long_id))
      {
        std::memcpy(out.long_payment_id, long_id.data, sizeof(long_id.data));
        out.long_payment_id_len = sizeof(long_id.data);
      }
      else if (cryptonote::get_encrypted_payment_id_from_tx_extra_nonce(nonce.nonce, short_id))
      {
        std::memcpy(out.short_payment_id, short_id.data, sizeof(short_id.data));
        out.has_short_payment_id = 1;
      }
    }

    cryptonote::tx_extra_tx_key_image_proofs proofs{};
    if (cryptonote::find_tx_extra_field_by_type(fields, proofs) && !proofs.proofs.empty())
    {
      copy_key(out.locked_key_image, proofs.proofs.front().key_image);
      out.has_locked_key_image = 1;
    }
  }

  int add_tx(lws_batch& batch, cryptonote::transaction&& tx, const crypto::hash& hash,
             const uint64_t* indices, uint32_t index_count, bool coinbase)
  {
    if (cryptonote::txversion::v4_tx_types < tx.version)
      return fail(LWS_ERR_UNSUPPORTED_TX, "unsupported tx version " +
        std::to_string(static_cast<unsigned>(tx.version)) + " in " + tools::type_to_hex(hash));
    if (index_count != tx.vout.size())
      return fail(LWS_ERR_INDICES, "daemon sent " + std::to_string(index_count) +
        " output indices for " + std::to_string(tx.vout.size()) + " outputs in " + tools::type_to_hex(hash));

    lws_tx out{};
    copy_key(out.hash, hash);
    copy_key(out.prefix_hash, cryptonote::get_transaction_prefix_hash(tx));
    out.version = static_cast<uint16_t>(tx.version);
    out.type = static_cast<uint16_t>(tx.type);
    out.unlock_time = tx.unlock_time;
    out.is_coinbase = coinbase ? 1 : 0;
    read_extra(tx, out);

    out.input_offset = checked_u32(batch.inputs.size());
    for (const auto& in : tx.vin)
    {
      const std::vector<uint64_t>* offsets = nullptr;
      lws_input entry{};
      if (const auto* key = std::get_if<cryptonote::txin_to_key>(&in))
      {
        entry.kind = LWS_INPUT_KEY;
        entry.amount = key->amount;
        copy_key(entry.key_image, key->k_image);
        offsets = &key->key_offsets;
      }
      else if (const auto* token = std::get_if<cryptonote::txin_zy_input>(&in))
      {
        entry.kind = LWS_INPUT_TOKEN;
        copy_key(entry.key_image, token->k_image);
        offsets = &token->key_offsets;
      }
      else if (std::holds_alternative<cryptonote::txin_gen>(in))
      {
        out.is_coinbase = 1;
        continue;
      }
      else
        continue; // script inputs never appear on the Beldex chain

      out.mixin = checked_u32(std::max<std::size_t>(1, offsets->size()) - 1);
      entry.ring_offset = checked_u32(batch.ring_members.size());
      entry.ring_count = checked_u32(offsets->size());
      uint64_t absolute = 0;
      for (const uint64_t relative : *offsets)
      {
        absolute += relative;
        batch.ring_members.push_back(absolute);
      }
      batch.inputs.push_back(entry);
    }
    out.input_count = checked_u32(batch.inputs.size() - out.input_offset);

    out.output_offset = checked_u32(batch.outputs.size());
    int32_t native = 0;
    for (std::size_t i = 0; i < tx.vout.size(); ++i)
    {
      const cryptonote::tx_out& vout = tx.vout[i];
      lws_output entry{};
      entry.index = checked_u32(i);
      entry.amount = vout.amount;
      entry.global_index = indices[i];
      entry.unlock_time = tx.get_unlock_time(i);
      entry.native_index = -1;
      if (const auto* key = std::get_if<cryptonote::txout_to_key>(&vout.target))
      {
        entry.kind = LWS_OUTPUT_KEY;
        copy_key(entry.pub_key, key->key);
        entry.native_index = native;
        if (static_cast<std::size_t>(native) < tx.rct_signatures.outPk.size())
          copy_key(entry.commitment, tx.rct_signatures.outPk[native].mask);
        ++native;
      }
      else if (const auto* token = std::get_if<cryptonote::tx_out_zyphora>(&vout.target))
      {
        entry.kind = LWS_OUTPUT_TOKEN;
        copy_key(entry.pub_key, token->stealth_address);
        copy_key(entry.commitment, token->amount_commitment);
        copy_key(entry.blinded_token_id, token->blinded_token_id);
        entry.encrypted_amount = token->encrypted_amount;
      }
      else
      {
        ++native; // script outputs still occupy a native rct slot
        continue;
      }
      batch.outputs.push_back(entry);
    }
    out.output_count = checked_u32(batch.outputs.size() - out.output_offset);

    batch.txs.push_back(out);
    batch.parsed.push_back(std::move(tx));
    return LWS_OK;
  }

  int parse_entry(lws_batch& batch, const lws_block_entry& entry, bool pruned)
  {
    cryptonote::block blk{};
    crypto::hash block_hash{};
    if (!cryptonote::parse_and_validate_block_from_blob(view(entry.block), blk, block_hash))
      return fail(LWS_ERR_BLOCK_PARSE, "failed to parse block blob");
    if (entry.tx_count != blk.tx_hashes.size())
      return fail(LWS_ERR_BLOCK_PARSE, "block " + tools::type_to_hex(block_hash) + " lists " +
        std::to_string(blk.tx_hashes.size()) + " txs but " + std::to_string(entry.tx_count) + " were sent");
    if (entry.index_count_len != entry.tx_count + 1)
      return fail(LWS_ERR_INDICES, "output index list does not cover every tx in block " +
        tools::type_to_hex(block_hash));

    lws_block out{};
    out.height = cryptonote::get_block_height(blk);
    out.timestamp = blk.timestamp;
    copy_key(out.hash, block_hash);
    copy_key(out.prev_hash, blk.prev_id);
    out.tx_offset = checked_u32(batch.txs.size());

    const uint64_t* indices = entry.indices;
    const crypto::hash miner_hash = cryptonote::get_transaction_hash(blk.miner_tx);
    if (const int rc = add_tx(batch, std::move(blk.miner_tx), miner_hash, indices, entry.index_counts[0], true))
      return rc;
    indices += entry.index_counts[0];

    for (std::size_t i = 0; i < entry.tx_count; ++i)
    {
      cryptonote::transaction tx{};
      const bool ok = pruned ?
        cryptonote::parse_and_validate_tx_base_from_blob(view(entry.txs[i]), tx) :
        cryptonote::parse_and_validate_tx_from_blob(view(entry.txs[i]), tx);
      if (!ok)
        return fail(LWS_ERR_TX_PARSE, "failed to parse tx " + tools::type_to_hex(blk.tx_hashes[i]) +
          " in block " + std::to_string(out.height));
      if (const int rc = add_tx(batch, std::move(tx), blk.tx_hashes[i], indices, entry.index_counts[i + 1], false))
        return rc;
      indices += entry.index_counts[i + 1];
    }

    out.tx_count = checked_u32(batch.txs.size() - out.tx_offset);
    batch.tx_height.resize(batch.txs.size(), out.height);
    batch.blocks.push_back(out);
    return LWS_OK;
  }

  //! Decrypts an 8-byte payment id with this account's derivation.
  void decrypt_short_id(uint8_t (&id)[8], const crypto::key_derivation& derivation)
  {
    char data[33];
    std::memcpy(data, &derivation, 32);
    data[32] = static_cast<char>(cryptonote::hashkey::ENCRYPTED_PAYMENT_ID);
    crypto::hash hash;
    crypto::cn_fast_hash(data, sizeof(data), hash);
    for (std::size_t i = 0; i < 8; ++i)
      id[i] ^= static_cast<uint8_t>(hash.data[i]);
  }

  void encode_rct(const crypto::key_derivation& derivation, uint32_t output_index, uint64_t amount,
                  const rct::key& mask, uint8_t out[96]);

  //! Bulletproof2-style compact ecdh decode, verified against the commitment.
  bool decode_native(const cryptonote::transaction& tx, const lws_output& out,
                     const crypto::key_derivation& derivation, uint64_t& amount, rct::key& mask)
  {
    const auto slot = static_cast<std::size_t>(out.native_index);
    if (out.native_index < 0 || tx.rct_signatures.ecdhInfo.size() <= slot ||
        tx.rct_signatures.outPk.size() <= slot)
      return false;

    crypto::ec_scalar scalar{};
    crypto::derivation_to_scalar(derivation, out.index, scalar);
    rct::ecdhTuple info = tx.rct_signatures.ecdhInfo[slot];
    rct::ecdhDecode(info, rct::sk2rct(reinterpret_cast<const crypto::secret_key&>(scalar)), true);

    rct::key check;
    rct::addKeys2(check, info.mask, info.amount, rct::H);
    if (!rct::equalKeys(check, tx.rct_signatures.outPk[slot].mask))
      return false;
    amount = rct::h2d(info.amount);
    mask = info.mask;
    return true;
  }

  void scan_tx(const lws_batch& batch, std::size_t tx_index, const lws_account* accounts,
               std::size_t account_count, uint64_t height, std::vector<lws_match>& matches)
  {
    const lws_tx& tx = batch.txs[tx_index];
    if (!tx.has_tx_pub_key || !tx.output_count)
      return;
    const cryptonote::transaction& parsed = batch.parsed[tx_index];
    const auto tx_pub = to_pod<crypto::public_key>(tx.tx_pub_key);

    for (std::size_t a = 0; a < account_count; ++a)
    {
      const lws_account& account = accounts[a];
      if (height <= account.scan_height)
        continue;

      crypto::key_derivation derivation{};
      if (!crypto::generate_key_derivation(tx_pub, to_pod<crypto::secret_key>(account.view_secret), derivation))
        continue;
      const auto spend_public = to_pod<crypto::public_key>(account.spend_public);

      bool short_id_ready = false;
      uint8_t short_id[8];

      for (uint32_t o = 0; o < tx.output_count; ++o)
      {
        const std::size_t output_index = tx.output_offset + o;
        const lws_output& out = batch.outputs[output_index];

        crypto::public_key derived{};
        if (!crypto::derive_subaddress_public_key(to_pod<crypto::public_key>(out.pub_key), derivation, out.index, derived) ||
            derived != spend_public)
          continue;

        lws_match match{};
        match.account = checked_u32(a);
        match.tx = checked_u32(tx_index);
        match.output = checked_u32(output_index);
        match.amount = out.amount;
        copy_key(match.mask, rct::identity());
        if (tx.is_coinbase)
          match.flags |= LWS_FLAG_COINBASE;

        if (out.kind == LWS_OUTPUT_TOKEN)
        {
          const auto* zout = std::get_if<cryptonote::tx_out_zyphora>(&parsed.vout[out.index].target);
          if (!zout)
            continue;
          uint64_t amount = 0;
          crypto::token_id token_id{};
          rct::key amount_mask{};
          rct::key blinding{};
          const cryptonote::account_keys view_only{};
          if (!cryptonote::decode_zyphora_output(view_only, *zout, derivation, out.index,
                                                 amount, token_id, amount_mask, blinding))
            continue; // commitment mismatch: corrupt or not ours
          match.amount = amount;
          copy_key(match.mask, amount_mask);
          copy_key(match.token_id, token_id);
          match.flags |= LWS_FLAG_RINGCT | LWS_FLAG_TOKEN;
        }
        else if (out.amount == 0 && !tx.is_coinbase && cryptonote::txversion::v1 < parsed.version)
        {
          uint64_t amount = 0;
          rct::key mask{};
          if (!decode_native(parsed, out, derivation, amount, mask))
            continue; // a matching key with a bad commitment is not spendable
          match.amount = amount;
          copy_key(match.mask, mask);
          match.flags |= LWS_FLAG_RINGCT;
        }

        if (match.flags & (LWS_FLAG_COINBASE | LWS_FLAG_RINGCT))
        {
          encode_rct(derivation, out.index, match.amount, to_pod<rct::key>(match.mask), match.rct);
          match.has_rct = 1;
        }

        if (tx.long_payment_id_len)
        {
          match.payment_id_len = tx.long_payment_id_len;
          std::memcpy(match.payment_id, tx.long_payment_id, sizeof(tx.long_payment_id));
        }
        else if (tx.has_short_payment_id)
        {
          if (!short_id_ready)
          {
            std::memcpy(short_id, tx.short_payment_id, sizeof(short_id));
            decrypt_short_id(short_id, derivation);
            short_id_ready = true;
          }
          match.payment_id_len = sizeof(short_id);
          std::memcpy(match.payment_id, short_id, sizeof(short_id));
        }
        matches.push_back(match);
      }
    }
  }

  //! commitment || ecdh-v1 encrypted mask || encrypted amount.
  void encode_rct(const crypto::key_derivation& derivation, uint32_t output_index, uint64_t amount,
                  const rct::key& mask, uint8_t out[96])
  {
    crypto::ec_scalar scalar{};
    crypto::derivation_to_scalar(derivation, output_index, scalar);
    rct::ecdhTuple encrypted{mask, rct::d2h(amount)};
    rct::ecdhEncode(encrypted, rct::sk2rct(reinterpret_cast<const crypto::secret_key&>(scalar)), false);
    const rct::key commitment = rct::commit(amount, mask);
    std::memcpy(out, commitment.bytes, 32);
    std::memcpy(out + 32, encrypted.mask.bytes, 32);
    std::memcpy(out + 64, encrypted.amount.bytes, 32);
  }

  template<typename F>
  int guarded(F&& f) noexcept
  {
    try
    {
      return f();
    }
    catch (const std::bad_alloc&)
    {
      return fail(LWS_ERR_INTERNAL, "out of memory");
    }
    catch (const std::exception& e)
    {
      return fail(LWS_ERR_INTERNAL, e.what());
    }
    catch (...)
    {
      return fail(LWS_ERR_INTERNAL, "unknown exception");
    }
  }
} // anonymous

extern "C" {

uint32_t lws_kernel_abi_version(void)
{
  return LWS_KERNEL_ABI_VERSION;
}

size_t lws_struct_sizes(size_t* out, size_t n)
{
  const size_t sizes[] = {
    sizeof(lws_key), sizeof(lws_blob), sizeof(lws_block_entry), sizeof(lws_block),
    sizeof(lws_tx), sizeof(lws_input), sizeof(lws_output), sizeof(lws_batch_view),
    sizeof(lws_account), sizeof(lws_match)
  };
  const size_t count = sizeof(sizes) / sizeof(sizes[0]);
  for (size_t i = 0; out && i < n && i < count; ++i)
    out[i] = sizes[i];
  return count;
}

size_t lws_last_error(char* out, size_t out_len)
{
  if (out && out_len)
  {
    const std::size_t n = std::min(out_len - 1, last_error.size());
    std::memcpy(out, last_error.data(), n);
    out[n] = '\0';
  }
  return last_error.size();
}

int lws_parse_blocks(const lws_block_entry* entries, size_t entry_count, int pruned, lws_batch** out)
{
  if (!out || (entry_count && !entries))
    return fail(LWS_ERR_ARGUMENT, "null argument");
  *out = nullptr;
  return guarded([&] {
    auto batch = std::make_unique<lws_batch>();
    batch->blocks.reserve(entry_count);
    for (std::size_t i = 0; i < entry_count; ++i)
    {
      const lws_block_entry& entry = entries[i];
      if ((entry.tx_count && !entry.txs) || !entry.index_counts || !entry.block.data)
        return fail(LWS_ERR_ARGUMENT, "malformed block entry");
      if (const int rc = parse_entry(*batch, entry, pruned != 0))
        return rc;
    }
    *out = batch.release();
    return LWS_OK;
  });
}

void lws_batch_get(const lws_batch* batch, lws_batch_view* out)
{
  if (!batch || !out)
    return;
  out->blocks = batch->blocks.data();
  out->block_count = batch->blocks.size();
  out->txs = batch->txs.data();
  out->tx_count = batch->txs.size();
  out->inputs = batch->inputs.data();
  out->input_count = batch->inputs.size();
  out->outputs = batch->outputs.data();
  out->output_count = batch->outputs.size();
  out->ring_members = batch->ring_members.data();
  out->ring_member_count = batch->ring_members.size();
}

void lws_batch_free(lws_batch* batch)
{
  delete batch;
}

int lws_scan(const lws_batch* batch, const lws_account* accounts, size_t account_count,
             lws_match** out, size_t* out_count)
{
  if (!batch || !out || !out_count || (account_count && !accounts))
    return fail(LWS_ERR_ARGUMENT, "null argument");
  *out = nullptr;
  *out_count = 0;
  return guarded([&] {
    std::vector<lws_match> matches;
    for (const lws_block& block : batch->blocks)
    {
      for (uint32_t t = 0; t < block.tx_count; ++t)
        scan_tx(*batch, block.tx_offset + t, accounts, account_count, block.height, matches);
    }
    if (matches.empty())
      return LWS_OK;
    auto* buffer = new lws_match[matches.size()];
    std::copy(matches.begin(), matches.end(), buffer);
    *out = buffer;
    *out_count = matches.size();
    return LWS_OK;
  });
}

void lws_matches_free(lws_match* matches)
{
  delete[] matches;
}

int lws_scan_pairs(const lws_batch* batch, const lws_account* accounts, size_t account_count,
                   const uint32_t* pair_tx, const uint32_t* pair_account, size_t pair_count,
                   lws_match** out, size_t* out_count)
{
  if (!batch || !out || !out_count || (pair_count && (!pair_tx || !pair_account || !accounts)))
    return fail(LWS_ERR_ARGUMENT, "null argument");
  *out = nullptr;
  *out_count = 0;
  return guarded([&] {
    std::vector<lws_match> matches;
    for (std::size_t i = 0; i < pair_count; ++i)
    {
      const uint32_t tx = pair_tx[i];
      const uint32_t account = pair_account[i];
      if (batch->txs.size() <= tx || account_count <= account)
        return fail(LWS_ERR_ARGUMENT, "pair out of range");
      const std::size_t before = matches.size();
      scan_tx(*batch, tx, &accounts[account], 1, batch->tx_height[tx], matches);
      for (std::size_t m = before; m < matches.size(); ++m)
        matches[m].account = account;
    }
    if (matches.empty())
      return LWS_OK;
    auto* buffer = new lws_match[matches.size()];
    std::copy(matches.begin(), matches.end(), buffer);
    *out = buffer;
    *out_count = matches.size();
    return LWS_OK;
  });
}

int lws_address_decode(const char* address, size_t len, uint8_t network,
                       lws_key* view_public, lws_key* spend_public)
{
  if (!address || !view_public || !spend_public)
    return fail(LWS_ERR_ARGUMENT, "null argument");
  const auto net = to_network(network);
  if (!net)
    return fail(LWS_ERR_ARGUMENT, "unknown network");
  return guarded([&] {
    cryptonote::address_parse_info info{};
    if (!cryptonote::get_account_address_from_str(info, *net, std::string_view{address, len}))
      return fail(LWS_ERR_ADDRESS, "invalid address for this network");
    if (info.is_subaddress || info.has_payment_id)
      return fail(LWS_ERR_ADDRESS, "subaddresses and integrated addresses are not supported");
    copy_key(*view_public, info.address.m_view_public_key);
    copy_key(*spend_public, info.address.m_spend_public_key);
    return LWS_OK;
  });
}

size_t lws_address_encode(const lws_key* view_public, const lws_key* spend_public,
                          uint8_t network, char* out, size_t out_len)
{
  const auto net = to_network(network);
  if (!view_public || !spend_public || !net)
    return 0;
  try
  {
    const cryptonote::account_public_address address{
      to_pod<crypto::public_key>(*spend_public), to_pod<crypto::public_key>(*view_public)
    };
    const std::string text = cryptonote::get_account_address_as_str(*net, false, address);
    if (out && out_len)
    {
      const std::size_t n = std::min(out_len - 1, text.size());
      std::memcpy(out, text.data(), n);
      out[n] = '\0';
    }
    return text.size();
  }
  catch (...)
  {
    return 0;
  }
}

int lws_view_key_matches(const lws_key* view_secret, const lws_key* view_public)
{
  if (!view_secret || !view_public)
    return 0;
  crypto::public_key derived{};
  if (!crypto::secret_key_to_public_key(to_pod<crypto::secret_key>(*view_secret), derived))
    return 0;
  return derived == to_pod<crypto::public_key>(*view_public) ? 1 : 0;
}

int lws_secret_to_public(const lws_key* secret, lws_key* public_out)
{
  if (!secret || !public_out)
    return fail(LWS_ERR_ARGUMENT, "null argument");
  crypto::public_key pub{};
  if (!crypto::secret_key_to_public_key(to_pod<crypto::secret_key>(*secret), pub))
    return fail(LWS_ERR_CRYPTO, "not a canonical scalar");
  copy_key(*public_out, pub);
  return LWS_OK;
}

void lws_generate_keypair(lws_key* secret_out, lws_key* public_out)
{
  crypto::public_key pub{};
  crypto::secret_key sec{};
  crypto::generate_keys(pub, sec);
  if (secret_out)
    copy_key(*secret_out, sec);
  if (public_out)
    copy_key(*public_out, pub);
}

int lws_output_rct(const lws_key* tx_pub_key, const lws_key* view_secret, uint32_t output_index,
                   uint64_t amount, const lws_key* mask, uint8_t out[96])
{
  if (!tx_pub_key || !view_secret || !mask || !out)
    return fail(LWS_ERR_ARGUMENT, "null argument");
  return guarded([&] {
    crypto::key_derivation derivation{};
    if (!crypto::generate_key_derivation(to_pod<crypto::public_key>(*tx_pub_key),
                                         to_pod<crypto::secret_key>(*view_secret), derivation))
      return fail(LWS_ERR_CRYPTO, "generate_key_derivation failed");

    encode_rct(derivation, output_index, amount, to_pod<rct::key>(*mask), out);
    return LWS_OK;
  });
}

int lws_derive_output_key(const lws_key* tx_secret, const lws_key* view_public, uint32_t output_index,
                          const lws_key* spend_public, lws_key* tx_pub_out, lws_key* output_key_out)
{
  if (!tx_secret || !view_public || !spend_public || !tx_pub_out || !output_key_out)
    return fail(LWS_ERR_ARGUMENT, "null argument");
  return guarded([&] {
    const auto r = to_pod<crypto::secret_key>(*tx_secret);
    crypto::public_key R{};
    if (!crypto::secret_key_to_public_key(r, R))
      return fail(LWS_ERR_CRYPTO, "tx secret is not a canonical scalar");
    crypto::key_derivation d{};
    if (!crypto::generate_key_derivation(to_pod<crypto::public_key>(*view_public), r, d))
      return fail(LWS_ERR_CRYPTO, "generate_key_derivation failed");
    crypto::public_key P{};
    if (!crypto::derive_public_key(d, output_index, to_pod<crypto::public_key>(*spend_public), P))
      return fail(LWS_ERR_CRYPTO, "derive_public_key failed");
    copy_key(*tx_pub_out, R);
    copy_key(*output_key_out, P);
    return LWS_OK;
  });
}

int lws_commit(uint64_t amount, const lws_key* mask, lws_key* out)
{
  if (!mask || !out)
    return fail(LWS_ERR_ARGUMENT, "null argument");
  return guarded([&] {
    copy_key(*out, rct::commit(amount, to_pod<rct::key>(*mask)));
    return LWS_OK;
  });
}

} // extern "C"
