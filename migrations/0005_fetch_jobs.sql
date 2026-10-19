-- Automated fetching: one job per request that the admin sends to the
-- worker, the hashes marked "didn't play", and a small key/value table for
-- the worker's kill switch and mode. Nothing here changes `requests`, so no
-- table rebuild. Both new tables cascade from `requests`, so deleting a
-- request (by the admin or by its requester) takes its job and blocked
-- hashes with it.
CREATE TABLE fetch_jobs (
  id              INTEGER PRIMARY KEY,
  request_id      INTEGER NOT NULL UNIQUE REFERENCES requests(id) ON DELETE CASCADE,
  state           TEXT NOT NULL DEFAULT 'queued'
                  CHECK (state IN ('queued','fetching','needs_pick','failed','done')),
  infuse_pro      INTEGER NOT NULL CHECK (infuse_pro IN (0,1)),   -- copied from the request
  claim_id        TEXT,                    -- set on claim, must accompany every report
  claimed_at      INTEGER,                 -- refreshed by progress calls; drives the reclaim
  attempts        INTEGER NOT NULL DEFAULT 0,
  max_attempts    INTEGER NOT NULL DEFAULT 3,
  uncached_adds   INTEGER NOT NULL DEFAULT 0,
  candidates      TEXT,                    -- ranked shortlist, JSON
  top_infohash    TEXT,                    -- the worker's own best pick
  chosen_infohash TEXT,                    -- what was actually used
  picked          INTEGER NOT NULL DEFAULT 0 CHECK (picked IN (0,1)),
  rd_torrent_id   TEXT,
  reason          TEXT,
  last_error      TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  -- A pick stores the hash in the same write, so a picked job always has one.
  CHECK (picked = 0 OR chosen_infohash IS NOT NULL)
);
CREATE INDEX fetch_jobs_state_claimed ON fetch_jobs(state, claimed_at);

CREATE TABLE blocked_hashes (
  request_id  INTEGER NOT NULL REFERENCES requests(id) ON DELETE CASCADE,
  infohash    TEXT NOT NULL,
  created_at  INTEGER NOT NULL,
  PRIMARY KEY (request_id, infohash)
);

CREATE TABLE settings (
  key    TEXT PRIMARY KEY,
  value  TEXT NOT NULL
);
