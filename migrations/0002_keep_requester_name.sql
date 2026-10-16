-- A member can be deleted while their requests stay. `requested_by`
-- becomes nullable and `requested_by_name` keeps the name once the member
-- row is gone. SQLite cannot drop NOT NULL in place, so the table is
-- rebuilt (new table, copy, drop old, rename). request_events references
-- requests, and dropping a parent that still has child rows leaves FK
-- violations SQLite never re-counts after the rename, so the events are
-- parked in a plain table and request_events is recreated afterwards.
PRAGMA defer_foreign_keys = ON;

CREATE TABLE requests_new (
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
  requested_by  INTEGER REFERENCES members(id),   -- NULL once the member is deleted
  requested_by_name TEXT,                         -- set when requested_by is cleared
  created_at    INTEGER NOT NULL,
  status_changed_at INTEGER NOT NULL,
  UNIQUE (tmdb_id, media_type),
  CHECK (requested_by IS NOT NULL OR requested_by_name IS NOT NULL)
);
INSERT INTO requests_new (id, tmdb_id, media_type, imdb_id, title, year, poster_path, note,
                          status, requested_by, created_at, status_changed_at)
  SELECT id, tmdb_id, media_type, imdb_id, title, year, poster_path, note,
         status, requested_by, created_at, status_changed_at FROM requests;

-- Park the events in a plain table so no child row points at `requests`
-- while it is dropped.
CREATE TABLE events_tmp AS SELECT * FROM request_events;
DROP TABLE request_events;
DROP TABLE requests;
ALTER TABLE requests_new RENAME TO requests;
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
INSERT INTO request_events (id, request_id, kind, actor, from_status, to_status, note, created_at)
  SELECT id, request_id, kind, actor, from_status, to_status, note, created_at FROM events_tmp;
DROP TABLE events_tmp;
CREATE INDEX events_request ON request_events(request_id, id);
