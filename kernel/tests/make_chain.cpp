// Generates a small testnet chain fixture for the end-to-end tests, as JSON on
// stdout. Blocks are real consensus-format blobs built with beldex core; they
// are not mined (the server never checks proof of work, the daemon does).
//
// Chain "a" (heights 0..10):
//   3, 5   coinbase outputs to the test wallet
//   8      a ringct transfer whose ring includes the wallet's height-3 output,
//          paying the wallet change (encrypted amount) and someone else
// Chain "b" shares 0..6 and replaces 7.. with blocks paying someone else, plus
// one more block, so switching a daemon from a to b is a 4-block reorg.

#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

#include "crypto/crypto.h"
#include "cryptonote_basic/cryptonote_basic.h"
#include "cryptonote_basic/cryptonote_basic_impl.h"
#include "cryptonote_basic/cryptonote_format_utils.h"
#include "cryptonote_basic/tx_extra.h"
#include "cryptonote_config.h"
#include "ringct/rctOps.h"

namespace
{
  struct keys
  {
    crypto::public_key view_pub, spend_pub;
    crypto::secret_key view_sec, spend_sec;
    keys()
    {
      crypto::generate_keys(view_pub, view_sec);
      crypto::generate_keys(spend_pub, spend_sec);
    }
  };

  std::string hex(const void* data, std::size_t n)
  {
    static const char* digits = "0123456789abcdef";
    std::string out;
    const auto* p = static_cast<const unsigned char*>(data);
    for (std::size_t i = 0; i < n; ++i)
    {
      out.push_back(digits[p[i] >> 4]);
      out.push_back(digits[p[i] & 15]);
    }
    return out;
  }
  template<typename T> std::string hex(const T& pod) { return hex(&pod, sizeof(pod)); }
  std::string hex(const std::string& s) { return hex(s.data(), s.size()); }

  struct built_block
  {
    std::string blob;
    std::vector<std::string> txs;
    std::vector<std::vector<uint64_t>> indices;
    crypto::hash hash;
  };

  struct chain_builder
  {
    uint64_t next_index = 0;
    std::vector<built_block> blocks;

    crypto::public_key pay(const keys& to, const crypto::secret_key& r, std::size_t index, crypto::key_derivation& d)
    {
      crypto::generate_key_derivation(to.view_pub, r, d);
      crypto::public_key out{};
      crypto::derive_public_key(d, index, to.spend_pub, out);
      return out;
    }

    void add_block(const keys& miner, uint64_t reward, uint64_t timestamp, const cryptonote::transaction* extra_tx)
    {
      const uint64_t height = blocks.size();
      cryptonote::block b{};
      b.major_version = cryptonote::hf::hf7;
      b.timestamp = timestamp;
      b.prev_id = blocks.empty() ? crypto::null_hash : blocks.back().hash;

      crypto::public_key R{};
      crypto::secret_key r{};
      crypto::generate_keys(R, r);
      crypto::key_derivation d{};
      cryptonote::transaction& m = b.miner_tx;
      m.version = cryptonote::txversion::v4_tx_types;
      m.type = cryptonote::txtype::standard;
      m.vin.push_back(cryptonote::txin_gen{static_cast<size_t>(height)});
      m.vout.push_back({reward, cryptonote::txout_to_key{pay(miner, r, 0, d)}});
      m.output_unlock_times = {height + 30};
      cryptonote::add_tx_extra<cryptonote::tx_extra_pub_key>(m, R);
      m.rct_signatures.type = rct::RCTType::Null;

      built_block out;
      out.indices.push_back({next_index++});
      if (extra_tx)
      {
        std::string blob = cryptonote::tx_to_blob(*extra_tx);
        crypto::hash h;
        cryptonote::get_blob_hash(blob, h);
        b.tx_hashes.push_back(h);
        std::vector<uint64_t> idx;
        for (std::size_t i = 0; i < extra_tx->vout.size(); ++i)
          idx.push_back(next_index++);
        out.indices.push_back(idx);
        out.txs.push_back(std::move(blob));
      }
      out.blob = cryptonote::block_to_blob(b);
      out.hash = cryptonote::get_block_hash(b);
      blocks.push_back(std::move(out));
    }
  };

  void print_chain(const char* name, const chain_builder& c, bool last)
  {
    std::printf("    \"%s\": [\n", name);
    for (std::size_t h = 0; h < c.blocks.size(); ++h)
    {
      const built_block& b = c.blocks[h];
      std::printf("      {\"height\": %zu, \"hash\": \"%s\", \"block\": \"%s\", \"txs\": [", h, hex(b.hash).c_str(), hex(b.blob).c_str());
      for (std::size_t t = 0; t < b.txs.size(); ++t)
        std::printf("%s\"%s\"", t ? ", " : "", hex(b.txs[t]).c_str());
      std::printf("], \"output_indices\": [");
      for (std::size_t t = 0; t < b.indices.size(); ++t)
      {
        std::printf("%s[", t ? ", " : "");
        for (std::size_t i = 0; i < b.indices[t].size(); ++i)
          std::printf("%s%llu", i ? ", " : "", static_cast<unsigned long long>(b.indices[t][i]));
        std::printf("]");
      }
      std::printf("]}%s\n", h + 1 < c.blocks.size() ? "," : "");
    }
    std::printf("    ]%s\n", last ? "" : ",");
  }
}

int main()
{
  const keys wallet, other;
  const uint64_t t0 = 1'700'000'000;
  const uint64_t reward = 2'000'000'000;
  const uint64_t change = 1'234'567'890;

  // The transfer at height 8, spending the wallet's height-3 coinbase output
  // (global index 3: one output per block before it).
  cryptonote::transaction transfer{};
  transfer.version = cryptonote::txversion::v4_tx_types;
  transfer.type = cryptonote::txtype::standard;
  cryptonote::txin_to_key in{};
  in.amount = 0;
  in.key_offsets = {1, 2, 2}; // absolute ring {1, 3, 5}: includes both wallet outputs
  crypto::public_key junk{};
  crypto::secret_key junk_sec{};
  crypto::generate_keys(junk, junk_sec);
  crypto::key_image image{};
  crypto::generate_key_image(junk, junk_sec, image);
  in.k_image = image;
  transfer.vin.push_back(in);

  crypto::public_key R{};
  crypto::secret_key r{};
  crypto::generate_keys(R, r);
  chain_builder scratch;
  crypto::key_derivation d_wallet{}, d_other{};
  transfer.vout.push_back({0, cryptonote::txout_to_key{scratch.pay(wallet, r, 0, d_wallet)}});
  transfer.vout.push_back({0, cryptonote::txout_to_key{scratch.pay(other, r, 1, d_other)}});
  transfer.output_unlock_times = {0, 0};
  cryptonote::add_tx_extra<cryptonote::tx_extra_pub_key>(transfer, R);
  transfer.rct_signatures.type = rct::RCTType::CLSAG;
  transfer.rct_signatures.txnFee = 10'000'000;
  for (std::size_t i = 0; i < 2; ++i)
  {
    const crypto::key_derivation& d = i == 0 ? d_wallet : d_other;
    const uint64_t amount = i == 0 ? change : 5'000'000;
    crypto::ec_scalar scalar{};
    crypto::derivation_to_scalar(d, i, scalar);
    const rct::key shared = rct::sk2rct(reinterpret_cast<const crypto::secret_key&>(scalar));
    const rct::key mask = rct::genCommitmentMask(shared);
    rct::ecdhTuple info{mask, rct::d2h(amount)};
    rct::ecdhEncode(info, shared, true);
    transfer.rct_signatures.ecdhInfo.push_back(info);
    transfer.rct_signatures.outPk.push_back(rct::ctkey{rct::zero(), rct::commit(amount, mask)});
  }
  transfer.pruned = true;

  chain_builder a, b;
  for (uint64_t h = 0; h <= 10; ++h)
  {
    const bool to_wallet = h == 3 || h == 5;
    a.add_block(to_wallet ? wallet : other, reward, t0 + h * 30, h == 8 ? &transfer : nullptr);
    if (h <= 6)
      b.blocks.push_back(a.blocks.back()), b.next_index = a.next_index;
  }
  for (uint64_t h = 7; h <= 11; ++h)
    b.add_block(other, reward, t0 + h * 30 + 7, nullptr);

  const cryptonote::account_public_address address{wallet.spend_pub, wallet.view_pub};
  std::printf("{\n  \"address\": \"%s\",\n  \"view_key\": \"%s\",\n  \"key_image\": \"%s\",\n",
              cryptonote::get_account_address_as_str(cryptonote::TESTNET, false, address).c_str(),
              hex(wallet.view_sec).c_str(), hex(image).c_str());
  std::printf("  \"reward\": %llu,\n  \"change\": %llu,\n  \"chains\": {\n",
              static_cast<unsigned long long>(reward), static_cast<unsigned long long>(change));
  print_chain("a", a, false);
  print_chain("b", b, true);
  std::printf("  }\n}\n");
  return 0;
}
