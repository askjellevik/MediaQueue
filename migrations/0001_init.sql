CREATE TABLE members (            -- one per person with an access code
  id            INTEGER PRIMARY KEY,
  name          TEXT NOT NULL,
  code_hash     BLOB NOT NULL UNIQUE,   -- HMAC-SHA256(CODE_PEPPER, normalised code)
  max_sessions  INTEGER NOT NULL DEFAULT 3,
  daily_limit   INTEGER,                -- NULL = use global default
  created_at    INTEGER NOT NULL,       -- unix seconds, UTC
  rotated_at    INTEGER,
  revoked_at    INTEGER,
  last_seen_at  INTEGER
);

CREATE TABLE sessions (
  id            INTEGER PRIMARY KEY,
  token_hash    BLOB NOT NULL UNIQUE,   -- sha256 of the cookie token
  kind          TEXT NOT NULL CHECK (kind IN ('user','admin')),
  member_id     INTEGER REFERENCES members(id),   -- NULL for admin
  created_at    INTEGER NOT NULL,
  expires_at    INTEGER NOT NULL,
  last_seen_at  INTEGER NOT NULL
);
CREATE INDEX sessions_expiry ON sessions(expires_at);
CREATE INDEX sessions_member ON sessions(member_id, created_at);

CREATE TABLE requests (
  id            INTEGER PRIMARY KEY,
  tmdb_id       INTEGER NOT NULL,
  media_type    TEXT NOT NULL CHECK (media_type IN ('movie','tv')),
  imdb_id       TEXT,
  title         TEXT NOT NULL,
  year          INTEGER,
  poster_path   TEXT,
  note          TEXT,                   -- max 200 chars, optional
  status        TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending','added','refused','already_had')),
  requested_by  INTEGER NOT NULL REFERENCES members(id),
  created_at    INTEGER NOT NULL,
  status_changed_at INTEGER NOT NULL,
  UNIQUE (tmdb_id, media_type)
);
CREATE INDEX requests_status_created ON requests(status, created_at DESC, id DESC);

CREATE TABLE request_events (          -- append-only archive and audit trail
  id          INTEGER PRIMARY KEY,
  request_id  INTEGER NOT NULL REFERENCES requests(id),
  kind        TEXT NOT NULL CHECK (kind IN ('submitted','duplicate','status')),
  actor       TEXT NOT NULL,             -- member name or 'admin'
  from_status TEXT, to_status TEXT,
  note        TEXT,
  created_at  INTEGER NOT NULL
);
CREATE INDEX events_request ON request_events(request_id, id);
