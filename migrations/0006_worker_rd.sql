-- What the Real-Debrid worker needs from the portal: a torrent the worker
-- is asked to delete (a title the admin marked as not playing), where a job
-- came from (the admin's button or the auto sweep), and which members
-- opted in to automatic fetching. Plain column additions; existing rows
-- get the defaults (no removal, origin `admin`, auto fetch off).
ALTER TABLE fetch_jobs ADD COLUMN remove_rd_torrent_id TEXT;
ALTER TABLE fetch_jobs ADD COLUMN origin TEXT NOT NULL DEFAULT 'admin'
  CHECK (origin IN ('admin','auto'));
ALTER TABLE members ADD COLUMN auto_fetch INTEGER NOT NULL DEFAULT 0
  CHECK (auto_fetch IN (0,1));
