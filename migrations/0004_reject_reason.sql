-- Why the admin refused a request, shown to the member. Optional: a
-- refused request without a reason simply has none, which is also what
-- every request that was refused before this migration gets. Cleared
-- whenever the request leaves `refused`.
ALTER TABLE requests ADD COLUMN reject_reason TEXT;
