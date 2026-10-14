## What is being built

Visitors with an access code search TMDB, pick a title and add it to a queue. You see the queue in an admin page, mark each entry **added**, **refused** or **already had**, and keep a permanent archive of everything ever submitted. A push notification tells you when something new arrives. You then find a good torrent yourself in Debrid Media Manager and add it to Real-Debrid, and Zurg exposes it to Infuse as before.

**Non-goals** (the CLI must not build these):

- No Real-Debrid key or integration anywhere in the app. It never touches your debrid account.
- No automatic torrent search or download, no Radarr/Sonarr/Prowlarr.
- No signup, password reset or profile pages for regular users.
- No comments, ratings, recommendations or social features.
- No media streaming or file hosting.

**Targets** (measured in Phase 7): idle RAM under 25 MB and under 50 MB at peak load with one Argon2 login in flight (measured, not assumed; if the limit proves too tight, raise the container limit rather than weakening Argon2), warm search under 10 ms server time, cold search under 800 ms, image under 50 MB, database writes only for logins, requests, status changes and one last-seen update per session per 10 minutes.

## Constraints and decisions

The stack is Rust (Axum on Tokio) with SQLite through SQLx, shipped as one binary in a distroless container, plus a no-build vanilla JS frontend embedded in that binary. Each choice below saves RAM, SD-card writes or attack surface.

| Area | Decision | Why |
| --- | --- | --- |
| Language and server | Rust stable, Axum on Tokio with 2 worker threads, `tower` and `tower-http` middleware | One binary, no interpreter or GC; expect 10–25 MB RSS |
| Database | SQLite in WAL mode via SQLx with only the `runtime-tokio`, `sqlite`, `macros`, `migrate` features; `query!` macros checked at compile time against committed offline metadata (`.sqlx/`) | Typos in SQL fail the build; Docker builds need no database |
| Outbound HTTP | One shared `reqwest::Client` with `default-features = false` and features `json`, `rustls-tls`, `http2` | rustls avoids OpenSSL on arm64 builds; pooling and keep-alive come built in. `reqwest` has no `http-client` feature |
| Caching | `moka` async cache with TTL and max capacity | TTL, size bound and request coalescing in one crate; plain `lru` has no TTL |
| Rate limiting | `governor` keyed limiters, a small bounded failure tracker, periodic cleanup, hard key cap | Well-tested algorithm; keyed state must be bounded by us |
| Frontend | Vanilla JS modules and one CSS file, embedded with `rust-embed`, no framework, no build step | One file to deploy, works on a read-only filesystem |
| User access | Short access code per person, exchanged for an opaque server-side session cookie | Easy to type and share by text; revocable per person |
| Admin auth | Argon2id via the `argon2` crate on the blocking pool behind a semaphore; optional Cloudflare Access in front | Strong login without letting hashing starve the Pi |
| Notifications | One HTTP POST to ntfy | No SMTP setup, push to phone |
| Deployment | Docker on the Pi (arm64), distroless `cc` nonroot image, published only on `127.0.0.1`, behind the existing Caddy | Loopback binding stops Docker from bypassing UFW |
| Hostname | Own subdomain such as `requests.askjellevik.no`, configurable | Separate from the streaming host |

**Environment the app lives in:**

- Raspberry Pi, Debian trixie, arm64. Assume SD or flash storage, so keep writes low.
- Caddy 2.7+ runs on the host (not in Docker) with `http_port 8081`, `https_port 8443`, the Cloudflare DNS plugin and `trusted_proxies` set to Cloudflare's ranges.
- UFW is active. Cloudflare sits in front of the DNS record.
- The portal's traffic is small JSON, so unlike the streaming host it can safely use the Cloudflare proxy (orange cloud).
- Compiling Axum, SQLx and reqwest can take many minutes on a Pi and may run out of memory when linking on small models. Build the arm64 image on a laptop or in CI (`docker buildx --platform linux/arm64`) and run it on the Pi.

## Architecture, layout and crates

One process serves the JSON API and the embedded static frontend. Caddy terminates TLS and forwards to the container on loopback; the app calls TMDB and ntfy outbound and writes one SQLite file on a mounted volume.

```text
media-requests/
  Cargo.toml  Cargo.lock  rust-toolchain.toml
  .sqlx/                 # offline query metadata, committed (cargo sqlx prepare)
  migrations/            # 0001_init.sql, ... (embedded via sqlx::migrate!)
  src/
    main.rs              # subcommands: serve, healthcheck, hash-password, add-member, backup
    config.rs            # env settings, validated at startup, fail fast
    state.rs             # AppState: pool, reqwest client, caches, limiters, config
    error.rs             # AppError -> JSON error responses
    db.rs                # pool, pragmas, ALL SQL lives here
    auth.rs              # codes, sessions, cookies, RequireUser / RequireAdmin extractors
    middleware.rs        # security headers, CSRF/Origin check, body limit, timeouts, catch-panic
    clientip.rs          # trusted-proxy client IP resolution
    ratelimit.rs         # governor keyed limiters, failure tracker, bounded maps
    tmdb.rs              # TMDB client, moka caches, single-flight
    notify.rs            # ntfy sender (fire and forget)
    routes/              # public.rs, admin.rs, static_files.rs
    tasks.rs             # hourly session cleanup, daily backup
  web/                   # index.html, admin.html, js/, css/  (embedded with rust-embed)
  tests/                 # integration tests: real app on a random port, mock TMDB and ntfy
  Dockerfile  compose.yaml  .env.example  README.md
```

**Code rules:**

- `#![forbid(unsafe_code)]`, `cargo fmt --check`, `cargo clippy -- -D warnings -D clippy::unwrap_used -D clippy::expect_used` in request paths. A handler must never panic; `tower-http`'s catch-panic layer is a safety net, not a plan.
- Handlers stay thin. All SQL lives in `db.rs`; write transactions start with `BEGIN IMMEDIATE` (`pool.begin_with("BEGIN IMMEDIATE")`) so SQLite never fails a lock upgrade mid-transaction.
- Never block the async runtime: Argon2 and any CPU-heavy work go through `spawn_blocking`. Set Tokio `max_blocking_threads` to 8 and the SQLx pool to at most 4 connections.
- Release profile: `lto = "thin"`, `codegen-units = 1`, `strip = true`, `panic = "unwind"` (so one bad request cannot kill the process). Commit `Cargo.lock` and build with `--locked`.
- Graceful shutdown on SIGTERM and Ctrl-C: stop accepting, drain in-flight requests, checkpoint the WAL, exit.
- The TMDB base URL and ntfy URL are configurable so tests can point at local mock servers.

**Approved crates** (anything else must be reported with name, version, features and reason, and approved before it is added; transitive dependencies of these crates, such as dashmap inside governor, need no separate approval and are not added directly): `axum`, `axum-extra` (cookie), `tokio`, `tower`, `tower-http` (set-header, limit, timeout, catch-panic), `serde`, `serde_json`, `sqlx`, `reqwest`, `argon2`, `sha2`, `hmac`, `subtle`, `rand`, `moka`, `governor`, `rust-embed`, `tracing`, `tracing-subscriber` (json), `clap`, `rpassword`, `thiserror`, `anyhow`, `csv`. Optional behind a cargo feature: `jsonwebtoken` (Cloudflare Access). Dev only: `wiremock`, `tokio-test`. Dependencies are audited with `cargo audit` and `cargo deny` before release.

## Data model

Four main tables plus a version table (SQLx's migrator keeps its own `_sqlx_migrations` table, so `schema_version` is not needed). `requests` holds one row per title; `request_events` is the append-only archive of every submission and status change. Nothing is ever hard-deleted.

```sql
-- Applied per connection in code (SqliteConnectOptions), not in the migration:
--   journal_mode=WAL, synchronous=NORMAL, foreign_keys=ON, busy_timeout=5000, temp_store=MEMORY

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
```

Behaviour that follows from the schema:

- A second person requesting the same title creates a `duplicate` event and no new row. Search results show it as already requested, without naming who asked.
- Re-requesting a refused title only logs a `duplicate` event. You can reopen it to `pending` from the admin page.
- Rotating a member's code replaces `code_hash` and deletes that member's sessions. Revoking sets `revoked_at` and deletes their sessions. Nothing about the old code survives.
- Logging in beyond `max_sessions` deletes the member's oldest session and logs the eviction, so the admin page can show devices per member.
- Expired sessions are deleted by a cleanup task once per hour. The archive tables are never pruned.
- Timestamps are unix seconds in UTC; the frontend formats them in the viewer's local time.
- To keep SD-card writes low, `last_seen_at` is updated at most once per 10 minutes per session or member. It is the only write a read-only request may cause.
- SQLx's SQLite macros infer nullability poorly, so queries use explicit overrides such as `col AS "col!"` where a column is known to be non-null.

## API spec

All endpoints return JSON, use `/api` as prefix, and send `Cache-Control: no-store`. Errors use one shape: `{"error": "code", "message": "..."}` with correct status codes (400, 401, 403, 404, 409, 429, 503). Every request body has a size cap (16 KB) and strict deserialisation (`deny_unknown_fields`).

| Method and path | Auth | Purpose | Notes |
| --- | --- | --- | --- |
| `POST /api/auth/code` | none | Body `{code}`; sets user session cookie, returns `{name}` | Failure-limited per IP plus global cap (section 7); same error for unknown, rotated and revoked codes |
| `POST /api/auth/logout` | user | Deletes session, clears cookie |  |
| `GET /api/me` | user | `{name, remaining_today}` | Frontend calls it on load: 401 shows the code gate |
| `GET /api/search?q=&type=` | user | Up to 8 slim results `{tmdb_id, type, title, year, poster, overview, state}` | `q` 2–80 chars; `type` is `all`, `movie` or `tv`; `state` is `none`, `requested`, `added`, `have` or `unavailable` |
| `POST /api/requests` | user | Body `{tmdb_id, media_type, note?}`; creates or logs duplicate | 201 new, 200 duplicate, 429 over quota. Server fetches title, year, poster and IMDb ID from TMDB itself; never trusts client-sent titles |
| `GET /api/requests/mine` | user | Own requests with current status | Cursor pagination |
| `POST /api/admin/login` | none | Body `{username, password}`; sets admin cookie | Failure-limited per IP plus global cap |
| `POST /api/admin/logout` | admin |  |  |
| `GET /api/admin/requests` | admin | Query `status` (`pending`, `added`, `refused`, `already_had`, `all`), `q`, `cursor`, `limit` | Cursor on `(created_at, id)`; includes requester name and IMDb ID |
| `PATCH /api/admin/requests/{id}` | admin | Body `{status}`; logs a `status` event | Any status to any other |
| `GET /api/admin/requests/{id}/events` | admin | Full history for one title |  |
| `GET /api/admin/export.csv` | admin | Whole archive as CSV | Streamed; guards against CSV formula injection (prefix `'` to cells starting with `=`, `+`, `-`, `@`) |
| `GET /api/admin/members` | admin | List members with last seen, active devices and request counts | Never returns codes |
| `POST /api/admin/members` | admin | Body `{name, daily_limit?, max_sessions?}`; returns the new code once | Code is generated server-side; only its HMAC is stored |
| `PATCH /api/admin/members/{id}` | admin | Body `{name?, daily_limit?, max_sessions?}` |  |
| `POST /api/admin/members/{id}/rotate` | admin | Returns a new code once; deletes that member's sessions | Use when a code leaks |
| `POST /api/admin/members/{id}/revoke` | admin | Revokes the member and deletes their sessions |  |
| `DELETE /api/admin/sessions/{id}` | admin | Kills one device session |  |
| `GET /healthz` | none | `{ok: true}` | Touches the DB with a trivial query; also used by the `healthcheck` subcommand |

Static pages: `/` (code gate, then search) and `/admin` (admin UI), both served from the embedded `web/` files. The admin HTML contains no data; all data comes from the admin API, which enforces the admin session.

## Authentication and security

Visitors type a short access code on a simple gate page; no accounts, no links to forward. Each code belongs to one named person, so you keep attribution, per-person limits and per-person revocation. The admin gets a real password login on top of an optional Cloudflare Access gate.

**Access codes and user sessions**

- Format: 8 characters from `23456789ABCDEFGHJKMNPQRSTUVWXYZ` (31 symbols, no look-alikes such as 0/O or 1/I/L), shown as `ABCD-EFGH`. That is about 40 bits. Generated with the OS random source using rejection sampling (no modulo bias). Length is configurable (`CODE_LENGTH`, default and minimum 8; the app refuses to start with a shorter length).
- Input is normalised before use: uppercase, spaces and hyphens removed. The same normalised string is what gets hashed.
- Storage: only `HMAC-SHA256(CODE_PEPPER, code)` is stored, in a unique-indexed column used for lookup, then compared in constant time with `subtle`. A plain hash would let someone with a leaked database brute-force a short code; the secret pepper (32+ random bytes in env) prevents that. If the database and the pepper leak together, a 40-bit code falls quickly, which is acceptable for this threat model.
- The code is shown once when you create or rotate a member, and cannot be read back. Lost code means a rotate.
- Admin-chosen codes: when you add or rotate a member you may type the code instead of having one generated (blank means generate). It is normalised as above, must be between `CODE_LENGTH` and 64 characters, may use any characters, and must not match another member's code (409). It is never echoed back. The same applies to `add-member --code`. A chosen code is only as strong as you make it: the 40-bit estimate below and the brute-force figures assume generated codes, so a guessable code is guessable despite the lockouts and throttle.
- A correct code creates a session: 32 random bytes in the cookie, SHA-256 of it in the database. Sessions last 90 days, sliding, and are deleted instantly on rotate or revoke.
- Wrong codes return one generic message with the same status and body whether the code never existed, was rotated or was revoked. Always compute the HMAC and do the lookup, so timing does not differ.

**Brute force and sharing**

- Per IP: 5 failed codes in 15 minutes triggers a lockout with exponential backoff (15 min, 1 h, 24 h). IPv6 clients are keyed by /64.
- Global throttle, not a block: above 60 failed codes in an hour, every code attempt is delayed 1 s before it is checked; above 300 per hour, 3 s. Delayed attempts hold a permit from a semaphore of 32, and when none is free the request gets 429 with `Retry-After`. A correct code still logs in, only slower, so friends are never locked out. Total guessing throughput stays at about 32 attempts per 3 s, which against about 40 bits would take thousands of years.
- Detection: when the throttle turns on or off, the app logs a warning and sends one ntfy alert (at most one per hour), so you notice an attack without watching logs.
- Codes shorter than 8 characters are not supported: a 6-digit code has only a million values, which a distributed attacker could cover.
- A code can be forwarded like any password. Each member has `max_sessions` (default 3), and a new login beyond that deletes the oldest session. The admin page shows active devices per member, and a leaked code costs you one rotate.

**Admin login**

- Username and Argon2id PHC hash come from env (`ADMIN_USER`, `ADMIN_PASSWORD_HASH`). `media-requests hash-password` prompts without echo and prints the hash; plain passwords are never stored or logged.
- Verification uses the `argon2` crate with Argon2id defaults (19 MiB, 2 iterations, 1 lane) inside `spawn_blocking`, guarded by a semaphore of 1 permit, so at most one 19 MiB hash runs at a time and a flood of logins queues instead of exhausting RAM. Raise the permit count only after measuring peak RSS during logins under mixed load, and never lower the Argon2 cost just to meet a RAM target. Unknown usernames still run a dummy verification.
- Admin session: 8 hour absolute lifetime and 2 hour idle timeout, new token on every login.
- Same failure tracking as code entry (5 per 15 minutes per IP, backoff, global throttle at 30 failures per 15 minutes).
- Optional hardening (Phase 7, behind a cargo feature and env switch): validate the `Cf-Access-Jwt-Assertion` header for `/admin` and `/api/admin` against your Cloudflare Access team keys. The app must work without it.

**Cookies and CSRF**

- Cookie names `__Host-mp_user` and `__Host-mp_admin`: `HttpOnly`, `Secure`, `SameSite=Strict`, `Path=/`, no `Domain`.
- Local development over plain HTTP: `COOKIE_DEV_INSECURE=1` drops the `__Host-` prefix and `Secure`. The app refuses to start with this flag when `PUBLIC_ORIGIN` is https.
- State-changing requests must be `application/json`, carry `X-Requested-With: mp`, and pass an `Origin` check against `PUBLIC_ORIGIN`. Reject otherwise with 403.

**Headers and input handling**

- CSP: `default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' https://image.tmdb.org; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'`. No inline scripts or styles.
- Also `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Permissions-Policy` denying camera, mic, geolocation, and `Cross-Origin-Opener-Policy: same-origin`.
- Strongly typed input: `tmdb_id` as `u32 > 0`, `media_type` as an enum, `note` as at most 200 characters with control characters stripped. SQL only through SQLx bound parameters.
- TMDB titles and overviews are untrusted: the frontend uses `textContent` only, never `innerHTML`.
- The app never logs cookies, codes, passwords or full request bodies.

**Client IP, resilience and network exposure**

- Caddy sets `X-Real-IP {client_ip}` (using your Cloudflare `trusted_proxies`). The app trusts that header only when the TCP peer (`ConnectInfo`) is in `TRUSTED_PROXY_CIDRS` (default loopback and the Docker bridge range). Client-supplied `X-Real-IP` or `X-Forwarded-For` is never believed.
- Middleware stack: 10 second request timeout, concurrency limit of 64, 16 KB body limit, catch-panic layer, structured request log without sensitive fields.
- Publish the container only as `127.0.0.1:8088:8000`. Docker's published ports bypass UFW, so loopback binding keeps Caddy as the only way in.
- Distroless image with no shell, non-root UID 65532, read-only root filesystem, all capabilities dropped, `no-new-privileges`. Only `/data` is writable.
- No secrets in the image or repo; everything comes from env, with `.env.example` committed and `.env` ignored.

## Rate limiting and TMDB caching

Limits are enforced in the app because stock Caddy cannot rate limit, and keyed by the person where possible so shared IPs (NAT, Cloudflare) do not punish innocent users. All values are env-configurable.

| Bucket | Key | Default | On exceed |
| --- | --- | --- | --- |
| Search | member id | burst 10, refill 5 per second | 429 with `Retry-After` |
| Create request | member id | 3 per minute and 10 per day (per-member override) | 429 with `remaining_today` |
| Wrong access code | IP (/64 for IPv6), plus global | 5 failures per 15 min with backoff; global throttle above 60 failures per hour | 429 per IP; the global throttle delays attempts instead of blocking |
| Wrong admin login | IP (/64 for IPv6), plus global | 5 failures per 15 min with backoff; same global throttle above 30 per 15 min | 429 |
| Any API call | IP | 120 per minute | 429 |
| TMDB upstream | global | 20 per second | Serve stale cache, or 503 |

Implementation requirements:

- Request-rate buckets use governor's built-in keyed limiter (`DefaultKeyedRateLimiter`, which brings its own dashmap internally; do not add dashmap as a direct dependency). A background task runs `retain_recent()` and `shrink_to_fit()` every minute, and a hard cap of 10,000 keys applies: a new key beyond the cap is treated as limited (fail closed), so memory cannot grow without bound under attack.
- Failure tracking (wrong code, wrong admin password) is a small struct per key with failure count, window start, lock-until and backoff level, kept in a `std::sync::Mutex<HashMap>` with short critical sections (never `.await` while locked) and the same key cap and cleanup. The global throttle is one shared counter over a sliding one-hour window plus the delayed-attempt semaphore from section 6.
- The daily request quota is counted from the database (`request_events` of kind `submitted`), not memory, so it survives restarts.
- Checks run before any TMDB or database work, and the global per-IP limiter runs before body parsing.
- Every 429 carries `Retry-After`. The frontend shows a friendly message and disables the input until then.
- Cloudflare's free rate-limit rule on `/api/auth/*` and `/api/admin/login` is a cheap extra layer. The app does not depend on it.

**TMDB caching and speed**

- Search calls `/search/multi` and keeps only `movie` and `tv` results (drops `person`). Cut to 8 results and slim the payload before sending to the browser.
- Time comes from an injectable `Clock` (system clock in production, fake clock in tests). All freshness decisions use it, not moka's own timers. moka's `time_to_live` (65 minutes for searches) is only memory cleanup.
- Search cache layout: `moka::future::Cache`, max 500 entries, key = normalised query (lowercase, trimmed, collapsed spaces) + type + epoch, where epoch = `clock.now / 600 s`. Value = `{results, fetched_at}`. An empty result list is a valid entry like any other.
- **Fresh:** an entry exists for the current epoch. Serve it, no upstream call.
- **Stale-usable:** no current-epoch entry, but an entry from one of the previous 6 epochs has a `fetched_at` under 60 minutes old. Try to refresh. If the refresh succeeds, the new entry replaces it. If it fails (network error, timeout, 5xx or 429), serve the stale entry and mark TMDB unhealthy for 30 seconds, during which stale entries are served with no upstream calls.
- **Gone:** nothing within 60 minutes. An upstream call is required; if it fails, return 503. An old entry is never served past 60 minutes.
- Single-flight: the current-epoch load goes through `try_get_with`, so concurrent callers share one upstream call. Failed loads are not cached.
- Any other 4xx from TMDB (for example 404 on a detail lookup) is never answered from stale data; it is returned as not found and the entry is removed.
- Detail lookups (title, year, IMDb ID) use the same three states with epoch = `now / 86400 s` and a hard limit of 7 days, since stale details are acceptable while TMDB is down.
- Short upstream timeouts (3 s connect, 3 s total) and one retry on connection errors only.
- `state` in each search result is computed with one `SELECT ... WHERE tmdb_id IN (...)` for all results, not one query per result.
- Posters are loaded by the browser directly from `image.tmdb.org` at small sizes (`w92` in the dropdown, `w185` in the confirm view), so the Pi never proxies images.
- IMDb ID comes from `/movie/{id}` (field `imdb_id`) or `/tv/{id}/external_ids`, fetched on submit, never during search.
- Show TMDB's required attribution (logo and the "not endorsed or certified by TMDB" notice) in the footer.

## Frontend and search experience

The search should feel as instant as Debrid Media Manager: type, see posters and titles within a blink, click one, confirm. No framework, no build step, plain ES modules served as static files with long-lived caching and hashed or versioned filenames.

**Access gate**

- On load the page calls `/api/me`. On 401 it renders only a centred code form and nothing else: one input, a Continue button and the app name. The search UI is not rendered until the session exists.
- The input is built for typing a short code on a phone: auto-uppercase, `autocapitalize="characters"`, `autocomplete="one-time-code"`, accepts pasted codes with or without the dash and spaces, and inserts the dash after four characters.
- Wrong code shows a neutral "That code didn't work". When locked out it shows a countdown from `Retry-After`. Nothing hints at which part was wrong.
- On success the page switches to the search view without a reload. The footer has a Log out link.

**Search behaviour**

- Start searching at 2 characters, debounce 200 ms, and cancel the previous request with `AbortController` so stale responses never overwrite newer ones.
- Dropdown shows up to 8 results: poster thumbnail (fixed width and height to avoid layout shift, `loading="lazy"`), title, year, a Movie or Series badge and a state chip (Requested, Added, Already available, Not available).
- Full keyboard support following the ARIA combobox pattern: arrow keys, Enter, Escape, Home and End, with `aria-activedescendant`. Works with touch and screen readers.
- Results already requested stay visible but their button is disabled, so people see why they cannot add them.
- Client-side memo of the last 20 queries makes backspacing instant.
- Skeleton rows while loading, a clear empty state, and an inline error with retry on failures.

**Confirm and submit**

- Selecting a result opens a small sheet: larger poster, title, year, trimmed overview, optional note field (200 characters, with counter), **Add to queue** and **Cancel**.
- Submit disables the button, shows a spinner and handles three outcomes: created, already requested, over quota (shows when they can request again).
- A "My requests" list under the search box shows each of their requests and its status, so people stop asking you what happened.

**Admin page**

- Tabs: **Pending**, **Added**, **Refused**, **Already had**, **Archive**, **Members**.
- Table rows: poster thumb, title and year, type, requester, requested time, IMDb ID with copy button, and an **Open in DMM** link built from a configurable base URL (expected pattern `/movie/<imdb_id>` and `/show/<imdb_id>`, to verify when building).
- One-click status buttons (Added, Refused, Already had, Reopen) with optimistic update and rollback on error. Filter box and cursor-based "load more".
- Archive tab shows every title ever requested with its event history in an expandable row, plus an **Export CSV** button.
- Members tab: add a member (name, optional daily limit and device limit); the new access code is shown once in a large, copyable box with a clear "you will not see this again" notice. Each row shows last seen, active devices (with a kill button per device) and request count, plus **Rotate code** and **Revoke**.

**Frontend rules**

- Mobile first, dark theme by default with `prefers-color-scheme` support, no web fonts, one CSS file.
- Never use `innerHTML` with API data; build nodes with `textContent`.
- Total transfer for the main page under 50 KB excluding posters. Gzip or zstd is handled by Caddy (`encode zstd gzip`).
- Footer carries the TMDB attribution.

## Notifications and your workflow

A new request triggers one ntfy push. Sending must never slow down or break a request: it runs as a background task with a 3 second timeout, and failures are logged and ignored.

- Config: `NTFY_URL` (for example `https://ntfy.sh` or your self-hosted instance), `NTFY_TOPIC`, optional `NTFY_TOKEN` sent as a bearer token. If `NTFY_URL` is empty, notifications are off.
- On public ntfy.sh anyone who knows the topic can read it, so use a long random topic name, or self-host ntfy and require a token.
- Message: title `New request: Dune (2021)`, body with type, requester name, note and the IMDb ID, `Click` header pointing to the admin pending tab, tag for movie or series.
- Only send for genuinely new titles, not for duplicates. Optional digest mode (one push per N minutes) is a later improvement, not part of v1.

**Your loop after a notification:**

1. Open the admin Pending tab and tap **Open in DMM** for the entry.
2. Pick a good torrent in Debrid Media Manager and add it to Real-Debrid.
3. Zurg picks it up; back in the portal tap **Added**. If it was already in the library tap **Already had**, if you will not add it tap **Refused**.
4. The requester sees the new status under "My requests".

## Docker, Caddy and operations

**Dockerfile:** two stages. The build stage is `rust:1-slim-bookworm` with `build-essential` and `pkg-config` (SQLite is compiled in by SQLx's bundled `libsqlite3-sys`), `SQLX_OFFLINE=true`, dependency layer caching, `cargo build --release --locked`. The final stage is `gcr.io/distroless/cc-debian12:nonroot` containing only the binary. There is no shell, curl or Python in the image, so the healthcheck is the binary itself.

```dockerfile
FROM rust:1-slim-bookworm AS build
WORKDIR /src
ENV SQLX_OFFLINE=true
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config \
 && rm -rf /var/lib/apt/lists/*
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && cp target/release/media-requests /media-requests

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /media-requests /app/media-requests
USER 65532:65532
ENTRYPOINT ["/app/media-requests"]
CMD ["serve"]
```

**compose.yaml requirements:**

```yaml
services:
  portal:
    image: media-requests:latest        # built on a laptop or CI for linux/arm64
    restart: unless-stopped
    env_file: .env
    ports: ["127.0.0.1:8088:8000"]     # loopback only, never 0.0.0.0
    volumes: ["./data:/data"]           # must be owned by UID 65532
    read_only: true
    tmpfs: ["/tmp:size=8m"]
    user: "65532:65532"
    cap_drop: [ALL]
    security_opt: ["no-new-privileges:true"]
    mem_limit: 64m
    cpus: 0.5
    pids_limit: 64
    logging: { driver: json-file, options: { max-size: "10m", max-file: "3" } }
    healthcheck:
      test: ["CMD", "/app/media-requests", "healthcheck"]
      interval: 30s
      timeout: 3s
      retries: 3
```

The `healthcheck` subcommand makes a plain HTTP GET to `127.0.0.1:8000/healthz` using only the standard library and exits 0 or 1. Run `sudo chown -R 65532:65532 data` once before the first start.

**Caddy site block** (added next to the existing media block; the host name is yours to choose):

```text
requests.askjellevik.no:8443 {
    encode zstd gzip
    request_body {
        max_size 64KB
    }
    reverse_proxy 127.0.0.1:8088 {
        header_up X-Real-IP {client_ip}
    }
}
```

The DNS record for this host can be proxied through Cloudflare (SSL mode Full strict). Open `8443/tcp` in UFW and the router only if you have not already, as for the media host.

**Operations:**

- Config in `.env` only: `PUBLIC_ORIGIN`, `BIND_ADDR`, `DATABASE_PATH`, `TMDB_TOKEN`, `TMDB_BASE_URL`, `ADMIN_USER`, `ADMIN_PASSWORD_HASH`, `CODE_PEPPER`, `CODE_LENGTH`, `DEFAULT_MAX_SESSIONS`, `NTFY_*`, `TRUSTED_PROXY_CIDRS`, `DMM_BASE_URL`, `COOKIE_DEV_INSECURE`, `RUST_LOG`, rate limit overrides. The app refuses to start if a required value is missing, `CODE_PEPPER` is under 32 bytes, or the origin is not https.
- Backups: a daily background task writes a consistent snapshot with `VACUUM INTO` to `/data/backups/`, keeping the last 14. Document how to copy them off the Pi. A `backup` subcommand does the same on demand.
- Logs: one JSON line per event through `tracing` (code entry success or failure, admin login, request created, status change, session eviction, rate-limit hit) with timestamp, IP and member name, never secrets.
- Updates: a documented rebuild-and-redeploy routine (`docker buildx build`, copy or push the image, `docker compose up -d`) and a note to rebuild monthly for base image security fixes.
- Migrations run automatically at startup through `sqlx::migrate!` and abort startup on failure.
