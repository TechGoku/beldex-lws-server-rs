-- Beldex LWS schema, PostgreSQL.
-- u64 chain values (amounts, unlock times) are stored bit-cast into BIGINT and
-- never aggregated in SQL; see lws-core.

CREATE TABLE lws_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- One row. `epoch` is bumped by every chain rollback; scanner commits check it
-- so a batch fetched before a reorg can never be written after one.
CREATE TABLE chain_state (
  id    SMALLINT PRIMARY KEY CHECK (id = 1),
  epoch BIGINT NOT NULL
);
INSERT INTO chain_state (id, epoch) VALUES (1, 0);

CREATE TABLE blocks (
  height BIGINT PRIMARY KEY,
  hash   BYTEA NOT NULL
);

CREATE TABLE accounts (
  id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  address      TEXT NOT NULL UNIQUE,
  view_public  BYTEA NOT NULL,
  spend_public BYTEA NOT NULL,
  view_key     BYTEA NOT NULL,
  status       SMALLINT NOT NULL,
  flags        SMALLINT NOT NULL,
  start_height BIGINT NOT NULL,
  scan_height  BIGINT NOT NULL,
  generation   BIGINT NOT NULL DEFAULT 0,
  created_at   BIGINT NOT NULL,
  last_access  BIGINT NOT NULL
);
CREATE INDEX accounts_by_status_height ON accounts (status, scan_height);

CREATE TABLE requests (
  kind         SMALLINT NOT NULL,
  address      TEXT NOT NULL,
  view_public  BYTEA NOT NULL,
  spend_public BYTEA NOT NULL,
  view_key     BYTEA NOT NULL,
  start_height BIGINT NOT NULL,
  flags        SMALLINT NOT NULL,
  created_at   BIGINT NOT NULL,
  PRIMARY KEY (kind, address)
);

CREATE TABLE outputs (
  account_id        BIGINT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
  ring_amount       BIGINT NOT NULL,
  global_index      BIGINT NOT NULL,
  height            BIGINT NOT NULL,
  tx_hash           BYTEA NOT NULL,
  tx_prefix_hash    BYTEA NOT NULL,
  tx_pub_key        BYTEA NOT NULL,
  out_index         INTEGER NOT NULL,
  amount            BIGINT NOT NULL,
  mask              BYTEA NOT NULL,
  pub_key           BYTEA NOT NULL,
  unlock_time       BIGINT NOT NULL,
  timestamp         BIGINT NOT NULL,
  mixin             INTEGER NOT NULL,
  flags             SMALLINT NOT NULL,
  payment_id        BYTEA,
  locked_key_image  BYTEA,
  token_id          BYTEA,
  blinded_token_id  BYTEA,
  amount_commitment BYTEA,
  encrypted_amount  BIGINT,
  rct               BYTEA,
  PRIMARY KEY (account_id, ring_amount, global_index)
);
CREATE UNIQUE INDEX outputs_by_pub_key ON outputs (account_id, pub_key);
CREATE INDEX outputs_by_height ON outputs (account_id, height, tx_hash, out_index);
-- Ring-member lookups for spend detection.
CREATE INDEX outputs_by_global_index ON outputs (global_index);

CREATE TABLE spends (
  account_id   BIGINT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
  key_image    BYTEA NOT NULL,
  source_amount BIGINT NOT NULL,
  source_index BIGINT NOT NULL,
  height       BIGINT NOT NULL,
  tx_hash      BYTEA NOT NULL,
  timestamp    BIGINT NOT NULL,
  unlock_time  BIGINT NOT NULL,
  mixin        INTEGER NOT NULL,
  payment_id   BYTEA,
  PRIMARY KEY (account_id, key_image, source_amount, source_index)
);
CREATE INDEX spends_by_height ON spends (account_id, height, tx_hash, key_image);
CREATE INDEX spends_by_source ON spends (account_id, source_amount, source_index);

-- Coordination between scanner processes (one holder per shard).
CREATE TABLE leases (
  name       TEXT PRIMARY KEY,
  holder     TEXT NOT NULL,
  expires_at BIGINT NOT NULL
);
