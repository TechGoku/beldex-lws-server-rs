// Diagnostic: prints the hash and prefix hash beldex core computes for a
// transaction blob given as hex on the command line.
#include <cstdio>
#include <string>

#include "cryptonote_basic/cryptonote_format_utils.h"
#include <oxenc/hex.h>

int main(int argc, char** argv)
{
  if (argc != 2)
  {
    std::fprintf(stderr, "usage: %s <tx blob hex>\n", argv[0]);
    return 2;
  }
  const std::string hex{argv[1]};
  if (!oxenc::is_hex(hex))
    return std::fprintf(stderr, "not hex\n"), 2;
  const std::string blob = oxenc::from_hex(hex);
  cryptonote::transaction tx;
  if (!cryptonote::parse_and_validate_tx_from_blob(blob, tx))
    return std::fprintf(stderr, "parse failed\n"), 1;
  std::printf("hash        %s\nprefix_hash %s\nversion %u type %u vin %zu vout %zu\n",
              tools::type_to_hex(cryptonote::get_transaction_hash(tx)).c_str(),
              tools::type_to_hex(cryptonote::get_transaction_prefix_hash(tx)).c_str(),
              static_cast<unsigned>(tx.version), static_cast<unsigned>(tx.type), tx.vin.size(), tx.vout.size());
  return 0;
}
