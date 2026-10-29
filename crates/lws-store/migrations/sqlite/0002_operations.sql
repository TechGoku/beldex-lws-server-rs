-- Operations: an audit trail of administrative changes, and the processes of
-- a deployment reporting in, so one dashboard sees every replica and scanner.

CREATE TABLE audit_log (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  at     INTEGER NOT NULL,
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
  started_at   INTEGER NOT NULL,
  heartbeat_at INTEGER NOT NULL,
  stats        TEXT NOT NULL
);
