-- Operations: an audit trail of administrative changes, and the processes of
-- a deployment reporting in, so one dashboard sees every replica and scanner.

CREATE TABLE audit_log (
  id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  at     BIGINT NOT NULL,
  actor  TEXT NOT NULL,
  remote TEXT NOT NULL,
  action TEXT NOT NULL,
  detail TEXT NOT NULL
);
CREATE INDEX audit_log_by_at ON audit_log (at);

CREATE TABLE instances (
  name         TEXT PRIMARY KEY,
  role         TEXT NOT NULL,
  version      TEXT NOT NULL,
  started_at   BIGINT NOT NULL,
  heartbeat_at BIGINT NOT NULL,
  stats        TEXT NOT NULL
);

-- Address prefix search from the dashboard (LIKE 'prefix%').
CREATE INDEX accounts_by_address_prefix ON accounts (address text_pattern_ops);
