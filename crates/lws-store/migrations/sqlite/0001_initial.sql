-- Beldex LWS schema, SQLite. Mirrors migrations/postgres; keep them in step.

CREATE TABLE lws_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE chain_state (
  id    INTEGER PRIMARY KEY CHECK (id = 1),
  epoch INTEGER NOT NULL
);
INSERT INTO chain_state (id, epoch) VALUES (1, 0);

CREATE TABLE blocks (
  height INTEGER PRIMARY KEY,
  hash   BLOB NOT NULL
);

CREATE TABLE accounts (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  address      TEXT NOT NULL UNIQUE,
  view_public  BLOB NOT NULL,
  spend_public BLOB NOT NULL,
  view_key     BLOB NOT NULL,
  status       INTEGER NOT NULL,
  flags        INTEGER NOT NULL,
  start_height INTEGER NOT NULL,
  scan_height  INTEGER NOT NULL,
  generation   INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL,
  last_access  INTEGER NOT NULL
);
CREATE INDEX accounts_by_status_height ON accounts (status, scan_height);

CREATE TABLE requests (
  kind         INTEGER NOT NULL,
  address      TEXT NOT NULL,
  view_public  BLOB NOT NULL,
  spend_public BLOB NOT NULL,
  view_key     BLOB NOT NULL,
  start_height INTEGER NOT NULL,
  flags        INTEGER NOT NULL,
  created_at   INTEGER NOT NULL,
  PRIMARY KEY (kind, address)
);

CREATE TABLE outputs (
  account_id        INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
  ring_amount       INTEGER NOT NULL,
  global_index      INTEGER NOT NULL,
  height            INTEGER NOT NULL,
  tx_hash           BLOB NOT NULL,
  tx_prefix_hash    BLOB NOT NULL,
  tx_pub_key        BLOB NOT NULL,
  out_index         INTEGER NOT NULL,
  amount            INTEGER NOT NULL,
  mask              BLOB NOT NULL,
  pub_key           BLOB NOT NULL,
  unlock_time       INTEGER NOT NULL,
  timestamp         INTEGER NOT NULL,
  mixin             INTEGER NOT NULL,
  flags             INTEGER NOT NULL,
  payment_id        BLOB,
  locked_key_image  BLOB,
  token_id          BLOB,
  blinded_token_id  BLOB,
  amount_commitment BLOB,
  encrypted_amount  INTEGER,
  rct               BLOB,
  PRIMARY KEY (account_id, ring_amount, global_index)
) WITHOUT ROWID;
CREATE UNIQUE INDEX outputs_by_pub_key ON outputs (account_id, pub_key);
CREATE INDEX outputs_by_height ON outputs (account_id, height, tx_hash, out_index);
-- Ring-member lookups for spend detection.
CREATE INDEX outputs_by_global_index ON outputs (global_index);

CREATE TABLE spends (
  account_id    INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
  key_image     BLOB NOT NULL,
  source_amount INTEGER NOT NULL,
  source_index  INTEGER NOT NULL,
  height        INTEGER NOT NULL,
  tx_hash       BLOB NOT NULL,
  timestamp     INTEGER NOT NULL,
  unlock_time   INTEGER NOT NULL,
  mixin         INTEGER NOT NULL,
  payment_id    BLOB,
  PRIMARY KEY (account_id, key_image, source_amount, source_index)
) WITHOUT ROWID;
CREATE INDEX spends_by_height ON spends (account_id, height, tx_hash, key_image);
CREATE INDEX spends_by_source ON spends (account_id, source_amount, source_index);

CREATE TABLE leases (
  name       TEXT PRIMARY KEY,
  holder     TEXT NOT NULL,
  expires_at INTEGER NOT NULL
);
