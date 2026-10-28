// Kernel self-test. Builds real Beldex blocks and transactions with beldex core,
// then checks that the C ABI parses and scans them the way the node and wallet2
// would. Exits non-zero on the first failure.

#include "lws_kernel.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include "crypto/crypto.h"
#include "crypto/hash.h"
#include "cryptonote_basic/cryptonote_basic.h"
#include "cryptonote_basic/cryptonote_basic_impl.h"
#include "cryptonote_basic/cryptonote_format_utils.h"
#include "cryptonote_basic/tx_extra.h"
#include "cryptonote_config.h"
#include "ringct/rctOps.h"

namespace
{
  int failures = 0;

#define CHECK(cond)                                                           \
  do {                                                                        \
    if (!(cond)) {                                                            \
      std::fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #cond);   \
      ++failures;                                                             \
    }                                                                         \
  } while (0)

  template<typename T>
  lws_key key_of(const T& value)
  {
    static_assert(sizeof(T) == 32);
    lws_key out;
    std::memcpy(out.data, &value, 32);
    return out;
  }

  template<typename T>
  bool same(const lws_key& a, const T& b)
  {
    return std::memcmp(a.data, &b, 32) == 0;
  }

  struct wallet
  {
    crypto::public_key view_pub, spend_pub;
    crypto::secret_key view_sec, spend_sec;
    wallet()
    {
      crypto::generate_keys(view_pub, view_sec);
      crypto::generate_keys(spend_pub, spend_sec);
    }
    lws_account account(uint64_t scan_height) const
    {
      return lws_account{key_of(view_sec), key_of(spend_pub), scan_height};
    }
  };

  //! One-time output key for `to` from tx secret `r`, plus the derivation.
  crypto::public_key pay(const wallet& to, const crypto::secret_key& r, std::size_t index,
                         crypto::key_derivation& derivation)
  {
    CHECK(crypto::generate_key_derivation(to.view_pub, r, derivation));
    crypto::public_key out{};
    CHECK(crypto::derive_public_key(derivation, index, to.spend_pub, out));
    return out;
  }

  void test_keys()
  {
    lws_key sec, pub, again;
    lws_generate_keypair(&sec, &pub);
    CHECK(lws_secret_to_public(&sec, &again) == LWS_OK);
    CHECK(std::memcmp(pub.data, again.data, 32) == 0);
    CHECK(lws_view_key_matches(&sec, &pub) == 1);
    pub.data[0] ^= 1;
    CHECK(lws_view_key_matches(&sec, &pub) == 0);

    lws_key bad{};
    std::memset(bad.data, 0xff, 32); // not reduced mod l
    CHECK(lws_secret_to_public(&bad, &again) != LWS_OK);
  }

  void test_addresses()
  {
    const wallet w;
    for (uint8_t net : {uint8_t(LWS_NET_MAINNET), uint8_t(LWS_NET_TESTNET), uint8_t(LWS_NET_DEVNET)})
    {
      const lws_key view = key_of(w.view_pub), spend = key_of(w.spend_pub);
      char text[256];
      const size_t len = lws_address_encode(&view, &spend, net, text, sizeof(text));
      CHECK(0 < len && len < sizeof(text));

      const cryptonote::account_public_address expected{w.spend_pub, w.view_pub};
      const auto nettype = static_cast<cryptonote::network_type>(net);
      CHECK(cryptonote::get_account_address_as_str(nettype, false, expected) == text);

      lws_key view2, spend2;
      CHECK(lws_address_decode(text, len, net, &view2, &spend2) == LWS_OK);
      CHECK(std::memcmp(view2.data, view.data, 32) == 0);
      CHECK(std::memcmp(spend2.data, spend.data, 32) == 0);

      const uint8_t other = net == LWS_NET_MAINNET ? LWS_NET_TESTNET : LWS_NET_MAINNET;
      CHECK(lws_address_decode(text, len, other, &view2, &spend2) == LWS_ERR_ADDRESS);

      const std::string sub = cryptonote::get_account_address_as_str(nettype, true, expected);
      CHECK(lws_address_decode(sub.data(), sub.size(), net, &view2, &spend2) == LWS_ERR_ADDRESS);
    }
    lws_key v, s;
    CHECK(lws_address_decode("not-an-address", 14, LWS_NET_MAINNET, &v, &s) == LWS_ERR_ADDRESS);
  }

  struct fixture
  {
    wallet alice, bob;
    cryptonote::block block;
    cryptonote::transaction transfer;
    std::string block_blob;
    std::string transfer_blob;
    crypto::hash transfer_hash;
    crypto::key_image spent_image;
    crypto::hash8 short_id;
    rct::key transfer_mask;
    static constexpr uint64_t height = 12;
    static constexpr uint64_t coinbase_amount = 16'500'000'000ull;
    static constexpr uint64_t transfer_amount = 1'234'567'890ull;

    fixture()
    {
      // Miner tx: output 0 to alice, output 1 to bob.
      crypto::public_key R{};
      crypto::secret_key r{};
      crypto::generate_keys(R, r);
      cryptonote::transaction& miner = block.miner_tx;
      miner.version = cryptonote::txversion::v4_tx_types;
      miner.type = cryptonote::txtype::standard;
      miner.vin.push_back(cryptonote::txin_gen{height});
      crypto::key_derivation d{};
      miner.vout.push_back({coinbase_amount, cryptonote::txout_to_key{pay(alice, r, 0, d)}});
      miner.vout.push_back({7, cryptonote::txout_to_key{pay(bob, r, 1, d)}});
      miner.output_unlock_times = {height + 60, height + 60};
      cryptonote::add_tx_extra<cryptonote::tx_extra_pub_key>(miner, R);
      miner.rct_signatures.type = rct::RCTType::Null;

      // Transfer: one CLSAG input spending ring {3, 10, 11}, one rct output to
      // alice with an encrypted short payment id.
      crypto::public_key R2{};
      crypto::secret_key r2{};
      crypto::generate_keys(R2, r2);
      transfer.version = cryptonote::txversion::v4_tx_types;
      transfer.type = cryptonote::txtype::standard;
      cryptonote::txin_to_key in{};
      in.amount = 0;
      in.key_offsets = {3, 7, 1};
      crypto::public_key junk{};
      crypto::secret_key junk_sec{};
      crypto::generate_keys(junk, junk_sec);
      crypto::generate_key_image(junk, junk_sec, spent_image);
      in.k_image = spent_image;
      transfer.vin.push_back(in);

      crypto::key_derivation d2{};
      transfer.vout.push_back({0, cryptonote::txout_to_key{pay(alice, r2, 0, d2)}});
      transfer.output_unlock_times = {0};
      cryptonote::add_tx_extra<cryptonote::tx_extra_pub_key>(transfer, R2);

      for (std::size_t i = 0; i < 8; ++i)
        short_id.data[i] = static_cast<char>(0x10 + i);
      {
        // Encrypt the short id exactly as wallet2 does.
        crypto::hash8 encrypted = short_id;
        char data[33];
        std::memcpy(data, &d2, 32);
        data[32] = static_cast<char>(cryptonote::hashkey::ENCRYPTED_PAYMENT_ID);
        crypto::hash h;
        crypto::cn_fast_hash(data, 33, h);
        for (std::size_t i = 0; i < 8; ++i)
          encrypted.data[i] ^= h.data[i];
        std::string nonce;
        cryptonote::set_encrypted_payment_id_to_tx_extra_nonce(nonce, encrypted);
        CHECK(cryptonote::add_extra_nonce_to_tx_extra(transfer.extra, nonce));
      }

      crypto::ec_scalar scalar{};
      crypto::derivation_to_scalar(d2, 0, scalar);
      const rct::key shared = rct::sk2rct(reinterpret_cast<const crypto::secret_key&>(scalar));
      transfer_mask = rct::genCommitmentMask(shared);
      rct::ecdhTuple info{transfer_mask, rct::d2h(transfer_amount)};
      rct::ecdhEncode(info, shared, true);
      transfer.rct_signatures.type = rct::RCTType::CLSAG;
      transfer.rct_signatures.txnFee = 1000;
      transfer.rct_signatures.ecdhInfo = {info};
      transfer.rct_signatures.outPk = {rct::ctkey{rct::zero(), rct::commit(transfer_amount, transfer_mask)}};
      transfer.pruned = true;
      transfer_blob = cryptonote::tx_to_blob(transfer);
      cryptonote::get_blob_hash(transfer_blob, transfer_hash);

      block.major_version = cryptonote::hf::hf7;
      block.timestamp = 1'700'000'000;
      block.prev_id = crypto::hash{};
      block.prev_id.data[0] = 1;
      block.tx_hashes = {transfer_hash};
      block_blob = cryptonote::block_to_blob(block);
    }
  };

  void test_parse_and_scan()
  {
    const fixture f;
    const lws_blob tx_blob{reinterpret_cast<const uint8_t*>(f.transfer_blob.data()), f.transfer_blob.size()};
    const uint64_t indices[] = {10, 11, 42};
    const uint32_t counts[] = {2, 1};
    const lws_block_entry entry{
      {reinterpret_cast<const uint8_t*>(f.block_blob.data()), f.block_blob.size()},
      &tx_blob, 1, indices, counts, 2
    };

    lws_batch* batch = nullptr;
    CHECK(lws_parse_blocks(&entry, 1, 1, &batch) == LWS_OK);
    if (!batch)
    {
      char err[256];
      lws_last_error(err, sizeof(err));
      std::fprintf(stderr, "parse error: %s\n", err);
      ++failures;
      return;
    }

    lws_batch_view v{};
    lws_batch_get(batch, &v);
    CHECK(v.block_count == 1);
    CHECK(v.blocks[0].height == fixture::height);
    CHECK(v.blocks[0].timestamp == 1'700'000'000);
    CHECK(same(v.blocks[0].hash, cryptonote::get_block_hash(f.block)));
    CHECK(v.blocks[0].prev_hash.data[0] == 1);
    CHECK(v.tx_count == 2);
    CHECK(v.txs[0].is_coinbase == 1);
    CHECK(v.txs[1].is_coinbase == 0);
    CHECK(same(v.txs[1].hash, f.transfer_hash));
    CHECK(v.txs[1].mixin == 2);
    CHECK(v.txs[1].has_short_payment_id == 1);
    CHECK(v.output_count == 3);
    CHECK(v.outputs[0].global_index == 10 && v.outputs[1].global_index == 11 && v.outputs[2].global_index == 42);
    CHECK(v.outputs[0].unlock_time == fixture::height + 60);
    CHECK(v.input_count == 1);
    CHECK(same(v.inputs[0].key_image, f.spent_image));
    CHECK(v.inputs[0].ring_count == 3);
    CHECK(v.ring_member_count == 3);
    CHECK(v.ring_members[0] == 3 && v.ring_members[1] == 10 && v.ring_members[2] == 11);

    const lws_account accounts[] = {f.alice.account(0), f.bob.account(fixture::height), wallet{}.account(0)};
    lws_match* matches = nullptr;
    size_t count = 0;
    CHECK(lws_scan(batch, accounts, 3, &matches, &count) == LWS_OK);
    CHECK(count == 2); // bob is already scanned past this block; the stranger owns nothing
    if (count == 2)
    {
      const lws_match& coinbase = matches[0];
      CHECK(coinbase.account == 0 && coinbase.tx == 0 && coinbase.output == 0);
      CHECK(coinbase.amount == fixture::coinbase_amount);
      CHECK(coinbase.flags == LWS_FLAG_COINBASE);
      CHECK(same(coinbase.mask, rct::identity()));

      const lws_match& rct_out = matches[1];
      CHECK(rct_out.account == 0 && rct_out.tx == 1 && rct_out.output == 2);
      CHECK(rct_out.amount == fixture::transfer_amount);
      CHECK(rct_out.flags == LWS_FLAG_RINGCT);
      CHECK(same(rct_out.mask, f.transfer_mask));
      CHECK(rct_out.payment_id_len == 8);
      CHECK(rct_out.has_rct == 1 && coinbase.has_rct == 1);
      {
        uint8_t expected[96];
        const lws_key pub = v.txs[1].tx_pub_key;
        const lws_key vs = key_of(f.alice.view_sec);
        const lws_key m = key_of(f.transfer_mask);
        CHECK(lws_output_rct(&pub, &vs, 0, fixture::transfer_amount, &m, expected) == LWS_OK);
        CHECK(std::memcmp(expected, rct_out.rct, 96) == 0);
      }
      CHECK(std::memcmp(rct_out.payment_id, f.short_id.data, 8) == 0);
    }
    lws_matches_free(matches);

    // The REST encoding of that output must decrypt back to the same values.
    uint8_t rct_blob[96];
    const lws_key tx_pub = v.txs[1].tx_pub_key;
    const lws_key view_sec = key_of(f.alice.view_sec);
    const lws_key mask = key_of(f.transfer_mask);
    CHECK(lws_output_rct(&tx_pub, &view_sec, 0, fixture::transfer_amount, &mask, rct_blob) == LWS_OK);
    CHECK(std::memcmp(rct_blob, rct::commit(fixture::transfer_amount, f.transfer_mask).bytes, 32) == 0);
    {
      crypto::key_derivation d{};
      crypto::generate_key_derivation(reinterpret_cast<const crypto::public_key&>(tx_pub), f.alice.view_sec, d);
      crypto::ec_scalar scalar{};
      crypto::derivation_to_scalar(d, 0, scalar);
      rct::ecdhTuple info{};
      std::memcpy(info.mask.bytes, rct_blob + 32, 32);
      std::memcpy(info.amount.bytes, rct_blob + 64, 32);
      rct::ecdhDecode(info, rct::sk2rct(reinterpret_cast<const crypto::secret_key&>(scalar)), false);
      CHECK(rct::equalKeys(info.mask, f.transfer_mask));
      CHECK(rct::h2d(info.amount) == fixture::transfer_amount);
    }

    lws_batch_free(batch);
  }

  void test_rejects_bad_input()
  {
    const fixture f;
    const lws_blob tx_blob{reinterpret_cast<const uint8_t*>(f.transfer_blob.data()), f.transfer_blob.size()};
    const uint64_t indices[] = {10, 11, 42};
    lws_batch* batch = nullptr;

    // Index count that does not match the outputs: refuse rather than misattribute.
    const uint32_t short_counts[] = {1, 1};
    lws_block_entry entry{
      {reinterpret_cast<const uint8_t*>(f.block_blob.data()), f.block_blob.size()},
      &tx_blob, 1, indices, short_counts, 2
    };
    CHECK(lws_parse_blocks(&entry, 1, 1, &batch) == LWS_ERR_INDICES);
    CHECK(batch == nullptr);

    // Missing tx blob for a block that lists one.
    const uint32_t counts[] = {2, 1};
    entry = {{reinterpret_cast<const uint8_t*>(f.block_blob.data()), f.block_blob.size()}, nullptr, 0, indices, counts, 1};
    CHECK(lws_parse_blocks(&entry, 1, 1, &batch) == LWS_ERR_BLOCK_PARSE);

    // Garbage block.
    const uint8_t junk[] = {1, 2, 3};
    entry = {{junk, sizeof(junk)}, nullptr, 0, indices, counts, 1};
    CHECK(lws_parse_blocks(&entry, 1, 1, &batch) == LWS_ERR_BLOCK_PARSE);
    char err[128];
    CHECK(lws_last_error(err, sizeof(err)) > 0);
  }
} // anonymous

int main()
{
  CHECK(lws_kernel_abi_version() == LWS_KERNEL_ABI_VERSION);
  test_keys();
  test_addresses();
  test_parse_and_scan();
  test_rejects_bad_input();
  if (failures)
  {
    std::fprintf(stderr, "%d check(s) failed\n", failures);
    return 1;
  }
  std::puts("kernel selftest: all checks passed");
  return 0;
}
