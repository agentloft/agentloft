-- Agentloft's website: who has signed in, and the computers they linked.

-- One row per Google account that has signed in.
CREATE TABLE users (
  id TEXT PRIMARY KEY,                -- Google's subject ("sub"), stable per account
  email TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  picture TEXT NOT NULL DEFAULT '',
  created_ms INTEGER NOT NULL,
  last_login_ms INTEGER NOT NULL
);

-- Signed-in browsers. Only the SHA-256 of the cookie's secret is kept.
CREATE TABLE sessions (
  hash TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_ms INTEGER NOT NULL,
  expires_ms INTEGER NOT NULL
);
CREATE INDEX sessions_user ON sessions(user_id);

-- Computers linked to an account. `secret` is the computer's own key
-- (it signs tickets for it and proves its reports), sealed with the
-- SECRETS_KEY secret, so a copy of the database alone opens nothing.
CREATE TABLE machines (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  platform TEXT NOT NULL DEFAULT '',
  version TEXT NOT NULL DEFAULT '',
  url TEXT,                           -- where a browser opens it: https://desk.tailnet.ts.net
  secret TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  last_seen_ms INTEGER NOT NULL
);
CREATE INDEX machines_user ON machines(user_id);

-- A computer asking to be linked (like `gh auth login`): the daemon polls
-- with `device_code`; the browser it opened holds `key`; both show
-- `user_code` so the person can check they match. Only hashes are kept.
CREATE TABLE links (
  device_hash TEXT PRIMARY KEY,
  key_hash TEXT NOT NULL UNIQUE,
  user_code TEXT NOT NULL,
  name TEXT NOT NULL,
  platform TEXT NOT NULL DEFAULT '',
  ip TEXT NOT NULL DEFAULT '',
  created_ms INTEGER NOT NULL,
  expires_ms INTEGER NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending', -- pending | approved | denied
  user_id TEXT REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX links_ip ON links(ip, created_ms);
