# Architecture

MediaQueue is a small request portal. People with an access code search
TMDB and request movies and series. An admin works through the queue,
marks each request, and can get a push notification for every new one.
It is one Rust binary (Axum on Tokio, SQLite through sqlx) with the
frontend (vanilla JavaScript modules, no build step) embedded in it.

## Data model

Defined in `migrations/`; times are unix seconds in UTC.

| Table | Holds |
| --- | --- |
| `members` | One row per person: unique name, `code_hash` (HMAC of the access code), optional `daily_limit` (`NULL` means no limit), and `rotated_at`, `revoked_at`, `last_seen_at`. (An unused `max_sessions` column remains from the first schema.) |
| `sessions` | Logged-in devices. `token_hash` is the SHA-256 of the cookie token. `kind` is `user` (with a `member_id`) or `admin`. |
| `requests` | One row per title and Infuse PRO answer (`UNIQUE (tmdb_id, media_type, infuse_pro)`), so members with and without Infuse PRO get separate requests and the admin can pick a fitting release for each. `status` is `pending`, `added`, `already_had` or `refused`. Also the IMDb ID, title, year, poster path, an optional note of up to 200 characters, an optional `reject_reason` of up to 300 characters (the admin's explanation, kept only while the status is `refused`), and who asked first (`requested_by`, which becomes `NULL` when that member is deleted; `requested_by_name` then keeps their name as text). |
| `request_events` | History: `submitted`, `duplicate` and `status` events with the actor's name. Events are only ever added, and removed together with their request when the admin deletes it (or a member withdraws it). It is the audit trail, the CSV export source and the source of the request quotas, so deleting a request also frees that day's quota for the people who asked for it. |
| `fetch_jobs` | One job per request that the admin sends to the worker (`UNIQUE (request_id)`, removed with the request). `state` is `queued`, `fetching`, `needs_pick`, `failed` or `done`. Holds the Infuse PRO answer copied from the request, `claim_id` and `claimed_at` (set on claim, refreshed by progress calls), `attempts` against `max_attempts`, `uncached_adds`, the worker's ranked `candidates` (JSON), `top_infohash` (its own pick), `chosen_infohash` and `picked` (a human chose it; a picked job always has a hash), `rd_torrent_id`, and `reason` and `last_error` for diagnostics. |
| `blocked_hashes` | Infohashes marked "didn't play", per request (removed with the request). The worker gets them with a claim and skips them. |
| `settings` | Small key/value table for the worker kill switch (`worker_enabled`) and mode (`worker_mode`); `WORKER_ENABLED` and `WORKER_MODE` in the environment override it. |

A second person asking for a title that already exists with the same Infuse
PRO answer adds a `duplicate` event instead of a row. The UI is in Norwegian (bokmål), with every string in
`web/js/strings.js`. Status labels are "Venter", "Lagt til", "Har fra før" and
"Avvist"; the stored values stay as listed above. The CSV export and the
log lines stay in English.

## API

All routes except `/healthz` live under `/api`. State-changing requests need
`Content-Type: application/json`, `X-Requested-With: mp` and an `Origin`
equal to `PUBLIC_ORIGIN`, otherwise they get 403 (the log says which check
failed). Every `/api` response has `Cache-Control: no-store`.

| Route | Who | Purpose |
| --- | --- | --- |
| `POST /auth/code`, `POST /auth/logout` | anyone, member | Log in with an access code; log out. |
| `GET /me` | member | Name and, if the member has a daily limit, `remaining_today`. |
| `GET /search?q=` | member | TMDB search; each result carries `states.plain` and `states.pro`, the request state of each Infuse PRO variant (`none` if not requested). |
| `POST /requests`, `GET /requests/mine`, `DELETE /requests/{id}` | member | Request a title (body: `tmdb_id`, `media_type`, optional `note`, optional `infuse_pro`, default false); list one's own requests (cursor paging, `status=pending\|earlier`); withdraw one's own request while it is pending and nobody else asked for it (the row and its events are deleted, which also frees the daily-limit slot). |
| `GET /events` | member | Server-sent ticks when a request changes. |
| `POST /admin/login`, `POST /admin/logout` | anyone, admin | Admin session. |
| `GET /admin/requests`, `PATCH` and `DELETE /admin/requests/{id}`, `GET /admin/requests/{id}/events` | admin | Queue with filter and paging; change a status (a `refused` PATCH may carry a `reason`, which the requester sees under "Vis begrunnelse"; `refused` again with a different reason replaces it); delete a request with its history; a request's history. |
| `GET /admin/counts`, `GET /admin/events`, `GET /admin/export.csv` | admin | Per-status counts; live ticks; CSV of everything. |
| `POST /admin/requests/{id}/fetch` | admin | Hand a pending request to the worker: creates its job (201; 409 if it has one or is not pending). Without an IMDb ID the job waits in `needs_pick` with reason `no_imdb_id`. |
| `POST /admin/jobs/{id}/retry`, `POST /admin/jobs/{id}/cancel` | admin | Requeue a job (from `failed`, `needs_pick`, `fetching` or `done`, only while its request is pending and has an IMDb ID); end a `queued`, `fetching` or `needs_pick` job as `failed` with reason `cancelled`. Either one makes a running worker's next call stale. 409 in any other state. |
| `GET /admin/jobs?state=&limit=&cursor=` | admin | Jobs, newest first, with the request's title, year and type; `candidates` is the stored JSON parsed (`null` if it does not parse). `state` is one of the job states or `all`. Worker-supplied text (`candidates`, `last_error`) must be rendered with `textContent` only. |
| `GET /admin/worker`, `PUT /admin/worker` | admin | The kill switch and mode (`dry_run`, `assisted`, `auto`), with `enabled_from_env` and `mode_from_env` saying whether the environment pins them. `PUT` takes either or both fields and returns the effective values. |
| `GET/POST /admin/members`, `PATCH` and `DELETE /admin/members/{id}` (delete only for a revoked member; their requests stay under their name and their events are relabelled `<name> (deleted)`), `POST /admin/members/{id}/rotate`, `POST /admin/members/{id}/revoke`, `POST /admin/members/{id}/restore` (undo a revoke), `DELETE /admin/sessions/{id}` | admin | Manage members and devices. |
| `GET /healthz` | anyone | Database check, for the container healthcheck. |


### Internal API (the worker)

Served on a second listener, `INTERNAL_LISTEN`, which is never published to
the host or routed through Caddy; it exists only when `INTERNAL_LISTEN` and
`INTERNAL_TOKEN` are both set. Every route needs `Authorization: Bearer
<INTERNAL_TOKEN>` and answers JSON in the same error shape as `/api`. There
is no cookie, CSRF check or per-IP rate limit here. The limits are 10 s per
request, a 256 KiB body and 8 calls in flight. None of these paths exist on
the public port (404). Nothing in the user interface uses this API yet.

| Route | Purpose |
| --- | --- |
| `POST /internal/jobs/claim` | The oldest queued job (200: `id`, `claim_id`, `tmdb_id`, `imdb_id`, `media_type`, `title`, `year`, `infuse_pro`, `chosen_infohash`, `rd_torrent_id`, `blocked_hashes`), now `fetching`; 204 when there is nothing to do or the kill switch is off. The member's note and name are never included. |
| `POST /internal/jobs/{id}/progress` | `claim_id` plus any of `rd_torrent_id`, `chosen_infohash`, `uncached_adds` (0 to 5). Saves them and refreshes `claimed_at`, so a long wait is not reclaimed. Nothing is ticked. |
| `POST /internal/jobs/{id}/report` | `claim_id`, `state` (`needs_pick`, `failed` or `done`) and optionally `candidates` (at most 20 flat objects, 64 KiB), `top_infohash`, `chosen_infohash`, `rd_torrent_id`, `reason` (a fixed list) and `error` (500 characters). `done` marks the request `added` (or `already_had` for that reason) as actor `worker`, only if it is still pending. Ticks the admin, and the members when the request changed. |
| `GET /internal/settings` | `{ "enabled", "mode" }`, the kill switch and mode. |

Progress and report answer 200 `{ "ok": true }`, or: 404 for an unknown job
(the worker treats it like a stale claim); 409 `stale_claim` when the job is
not `fetching` under that `claim_id`, with the `rd_torrent_id` stored on the
job when there is one (the key is absent otherwise), so the worker keeps
that torrent and deletes only its own; 400 `picked_locked` when the job was
picked by a human and the call would change `chosen_infohash`; 400 for
anything malformed.

Anything else is served from the embedded `web/` directory.

## Authentication

- **Access codes.** Generated codes are 8 characters from a 31-symbol
  alphabet without look-alikes, shown as `ABCD-EFGH`, drawn from the OS
  random source. An admin may choose a code of 3 to 64 characters instead;
  short ones are guessable. Codes are normalised (upper case, spaces and
  hyphens removed) and only `HMAC-SHA256(CODE_PEPPER, code)` is stored.
- **Member sessions.** A login creates a session (one device) with a random
  token in a `__Host-` cookie; the database keeps its hash. Sessions last 90
  days, sliding. At 10 sessions (`MAX_SESSIONS_PER_MEMBER`) the oldest is dropped; the cap only bounds session rows. Rotating
  or revoking a member deletes their sessions.
- **Admin.** `ADMIN_USER` plus an Argon2id password hash from the
  environment. Sessions last 8 hours, or 2 hours idle. Hashing runs one at a
  time. Optionally, a Cloudflare Access token is also required (build with
  `--features cf-access`).
- **Client IP.** `X-Real-IP` is trusted only from `TRUSTED_PROXY_CIDRS`.

## Limits

These protect the server and TMDB; normal use does not reach them. They are
fixed in `RateLimitConfig::default()` (`src/config.rs`).

| Limit | Value |
| --- | --- |
| Any `/api` call | 120 a minute per IP (IPv6 by /64) |
| Search | burst 10, refill 5 a second, per member |
| Requests (new or duplicate) | 10 a minute per member |
| Daily requests | only if the admin set a limit for the member |
| Outbound TMDB calls | 20 a second in total |
| Wrong codes or admin logins | 5 in 15 minutes locks that IP for 15 minutes, then 1 hour, then 24 hours |
| Global login throttle | codes: above 60 failures an hour attempts wait 1 s, above 300 wait 3 s; admin: 30 and 150 in 15 minutes. Correct credentials still work. |
| Live streams | 200 at once |
| Per request | 10 s timeout, 16 KB body, 64 requests in flight |

## Live updates

`GET /api/events` and `GET /api/admin/events` are server-sent event
streams carrying content-free `tick` events. Status changes tick members and
admin; new requests tick the admin. Clients refetch the lists they show,
update rows in place and reconnect on their own, falling back to polling
every minute. Each stream re-checks its session every 25 seconds and ends
when it is gone. Ticks live in an in-process broadcast channel, so this
works for a single instance only.

## Static assets

`web/` is embedded with `rust_embed`; changing it needs a rebuild. HTML
pages reference assets as `/v/__V__/...`, and the server replaces `__V__`
with a hash over all embedded files. Only URLs with the current hash are
`immutable`; anything else revalidates by ETag.

## Configuration

Set in the environment (Docker Compose reads `.env`). The app refuses to
start if a required value is missing or invalid. Unknown variables are
ignored.

| Variable | Default | Purpose |
| --- | --- | --- |
| `PUBLIC_ORIGIN` | required | Exact address in the browser, without a trailing slash or path. An `http://` origin turns off the `Secure` cookie flag (local development only). |
| `CODE_PEPPER` | required | HMAC key for access codes, at least 32 bytes. Changing it invalidates every code. |
| `ADMIN_USER`, `ADMIN_PASSWORD_HASH` | required | Admin login; make the hash with `mediaqueue hash-password`. |
| `TMDB_TOKEN` | required | TMDB API read access token. |
| `NTFY_URL`, `NTFY_TOPIC`, `NTFY_TOKEN` | off | Push notification for new requests. A push that fails for a passing reason (timeout, connection error, 5xx, 429) is tried up to two more times, 2 s and 10 s later. |
| `CF_ACCESS_ENABLED`, `CF_ACCESS_TEAM_DOMAIN`, `CF_ACCESS_AUD`, `CF_ACCESS_CERTS_URL` | off | Cloudflare Access check (needs the `cf-access` build feature). |
| `INTERNAL_LISTEN`, `INTERNAL_TOKEN` | off | Address of the worker listener (must differ from `BIND_ADDR`) and its bearer token, at least 32 bytes. Both or neither: a token without an address is a startup error. |
| `WORKER_ENABLED`, `WORKER_MODE` | unset | Pin the kill switch (`true` or `false`) and the mode (`dry_run`, `assisted`, `auto`) so the admin toggle cannot change them. |
| `RD_TOKEN`, `RD_TOKEN_FILE` | must be unset | The portal refuses to start if either is set: the Real-Debrid token belongs to the worker service only. |
| `TRUSTED_PROXY_CIDRS` | loopback and `172.16.0.0/12` | Proxies trusted to send `X-Real-IP`. |
| `RUST_LOG` | `info` | Log filter. |
| `BIND_ADDR` | `0.0.0.0:8000` | Listen address (the image sets it). |
| `DATABASE_PATH` | `./data/db.sqlite3` | SQLite file (the image sets `/data/db.sqlite3`). |
| `TMDB_BASE_URL`, `DMM_BASE_URL` | the real services | For tests and mocks. `DMM_BASE_URL` builds the optional "Open in DMM" link on the admin page. |

## Performance targets

Measured at the compose limits (0.5 CPU, 64 MB) against a mock TMDB:

- idle memory under 25 MB, peak under 50 MB with Argon2 logins in flight;
- warm search under 10 ms, cold search under 800 ms;
- image under 50 MB;
- database writes only for logins, requests, status changes and one
  last-seen update per session every 10 minutes.
