# MediaQueue

A small self-hosted request portal. Friends type an access code, search
TMDB and add titles to a queue. You work through the queue on an admin
page and get a push notification for every new title. The app never talks
to Real-Debrid or any debrid service. You still pick torrents yourself in
Debrid Media Manager.

One Rust binary (Axum, SQLite) with the frontend embedded, shipped as a
distroless container for a Raspberry Pi behind Caddy. The full spec is in
[docs/SPEC.md](docs/SPEC.md), and the security review is in
[docs/SECURITY.md](docs/SECURITY.md).

## First run

Everything below happens in one directory on the Pi, for example
`~/media-requests`, holding `compose.yaml`, `.env` and `data/`.

1. **Get the image** onto the Pi (see "Build and deploy" below).

2. **Generate the secrets.**

   ```sh
   # 48 random bytes for the access-code HMAC key.
   openssl rand -base64 48

   # Argon2id hash of your admin password (prompts, no echo).
   docker run --rm -it media-requests:latest hash-password
   ```

3. **Create a TMDB token.** Sign in at themoviedb.org, open Settings > API,
   and copy the "API Read Access Token" (the long v4 token, not the short
   API key).

4. **Write `.env`** from `.env.example`. Every variable is documented
   there. The required ones are:

   ```sh
   PUBLIC_ORIGIN=https://requests.example.com
   CODE_PEPPER=<the openssl output>
   ADMIN_USER=admin
   ADMIN_PASSWORD_HASH='$argon2id$v=19$m=19456,t=2,p=1$...'
   TMDB_TOKEN=<the read access token>
   ```

   Keep the single quotes around `ADMIN_PASSWORD_HASH`. Docker Compose
   expands `$` in env files, which would mangle the hash, and the app
   would refuse to start.

   For push notifications, also set `NTFY_URL` and `NTFY_TOPIC` (and
   `NTFY_TOKEN` for a protected self-hosted ntfy). On public ntfy.sh, use
   a long random topic, since anyone who knows it can read it.

5. **Create the data directory**, owned by the container's user:

   ```sh
   mkdir -p data
   sudo chown -R 65532:65532 data
   ```

6. **Start it.**

   ```sh
   docker compose up -d
   docker compose ps          # wait for "(healthy)"
   docker compose logs -f
   ```

7. **Add the first member.** The code is printed once and cannot be shown
   again. If it is lost, rotate it on the admin page.

   ```sh
   docker compose exec portal /app/media-requests add-member --name Alice
   ```

   After that, add members on the admin page at `/admin` (Members tab).

## Build and deploy

Compiling on the Pi is slow and can run out of memory while linking, so
build the arm64 image on a laptop or in CI:

```sh
docker buildx build --platform linux/arm64 -t media-requests:latest --load .
docker save media-requests:latest | gzip | ssh pi 'gunzip | docker load'
```

To include the optional Cloudflare Access check, add
`--build-arg FEATURES=cf-access` (see "Cloudflare" below).

Then copy `compose.yaml` and `.env` to the Pi, if you haven't yet, and run
`docker compose up -d` there. The container listens only on
`127.0.0.1:8088`. Docker's published ports bypass UFW, so binding to
loopback keeps Caddy as the only way in.

## Caddy

Add a site block next to the existing media one:

```text
requests.example.com:8443 {
    encode zstd gzip
    request_body {
        max_size 64KB
    }
    reverse_proxy 127.0.0.1:8088 {
        header_up X-Real-IP {client_ip}
    }
}
```

The app trusts `X-Real-IP` only from `TRUSTED_PROXY_CIDRS` (loopback and
the Docker bridge range by default), and never trusts `X-Forwarded-For`.
Caddy's `trusted_proxies` must list Cloudflare's ranges so that
`{client_ip}` is the visitor, not Cloudflare. Otherwise every visitor
shares one rate-limit bucket.

## UFW, router and Cloudflare

- Open `8443/tcp` in UFW and forward it on the router, if the media host
  has not done that already. Port 8088 stays closed: it is loopback only.
- The DNS record can be proxied through Cloudflare (orange cloud, SSL
  mode Full (strict)), since the portal only serves small JSON responses.
- An optional free Cloudflare rate-limit rule on `/api/auth/*` and
  `/api/admin/login` adds another layer. The app does not depend on it.
- **Cloudflare Access** (optional): protect `/admin*` and `/api/admin*`
  with an Access application, build the image with
  `--build-arg FEATURES=cf-access`, and set `CF_ACCESS_ENABLED=1`,
  `CF_ACCESS_TEAM_DOMAIN` and `CF_ACCESS_AUD` in `.env`. The app then
  checks the `Cf-Access-Jwt-Assertion` header on top of the admin password.

## Backups and restore

A background task writes a consistent snapshot (`VACUUM INTO`) to
`data/backups/db-<UTC time>.sqlite3` once a day and keeps the newest 14.
To take one on demand:

```sh
docker compose exec portal /app/media-requests backup
```

Copy them off the Pi regularly:

```sh
rsync -av pi:media-requests/data/backups/ ~/backups/media-requests/
```

To restore:

```sh
docker compose down
mv data/db.sqlite3 data/db.sqlite3.broken
rm -f data/db.sqlite3-wal data/db.sqlite3-shm
cp data/backups/db-20260101T031500Z.sqlite3 data/db.sqlite3
sudo chown 65532:65532 data/db.sqlite3
docker compose up -d
```

Use the same `CODE_PEPPER` as before, or every access code stops working.
Keep a copy of `.env` with the backups.

## Updates

```sh
git pull
docker buildx build --platform linux/arm64 -t media-requests:latest --load .
docker save media-requests:latest | gzip | ssh pi 'gunzip | docker load'
ssh pi 'cd media-requests && docker compose up -d'
```

Migrations run at startup. Rebuild at least monthly even without code
changes, so the base image picks up Debian security fixes.

## Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| Container restarts at once; `docker compose logs` shows `configuration error` | A required variable is missing or invalid. The message names it. A mangled `ADMIN_PASSWORD_HASH` means the single quotes are missing. |
| `unable to open database file` | `data/` is not owned by 65532: `sudo chown -R 65532:65532 data`. |
| Every POST returns 403 | `PUBLIC_ORIGIN` does not match the address in the browser (scheme, host and port must match exactly). |
| Everyone gets locked out together | All visitors look like one IP. Check `header_up X-Real-IP {client_ip}` in Caddy, Caddy's `trusted_proxies`, and `TRUSTED_PROXY_CIDRS`. |
| Search says the movie database is not answering | TMDB is down or the token is wrong (`tmdb rejected the token` in the logs). Cached results keep working for up to an hour. |
| "Too many attempts" on the code page | Five wrong codes from one network lock it for 15 minutes, then longer. It ends on its own, or restart the container to clear all limits. |
| No push notifications | `NTFY_URL` empty means notifications are off. Failures are logged as `ntfy notification failed`. |
| Lost access code | Admin page, Members tab, **Rotate code**. |
| Running outside Docker uses more memory | Set `GLIBC_TUNABLES=glibc.malloc.mmap_threshold=131072` as the Dockerfile does, so Argon2's memory goes back to the OS. |

## Development

```sh
cp .env.example .env    # set COOKIE_DEV_INSECURE=1, PUBLIC_ORIGIN=http://localhost:8000
cargo test                                   # unit and integration tests
cargo test --features cf-access              # plus the Cloudflare Access tests
cargo test --test browser -- --ignored       # headless Chrome; CHROME=/path/to/chrome
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo audit && cargo deny check
```

SQL is checked at compile time against `.sqlx/`. After changing a query,
regenerate it against a migrated scratch database:

```sh
export DATABASE_URL=sqlite://$PWD/dev.sqlite3
sqlx database create && sqlx migrate run
cargo sqlx prepare -- --all-targets --all-features
```

### Load test

`scripts/loadtest.sh` fires 200 mixed requests: searches, `/api/me`, new
requests, and admin logins with right and wrong passwords. It reports
status codes, p50/p95 latency, sampled peak RSS and the database writes
the run caused. Run it against a mock TMDB, never the real one:

```sh
cargo run --release --example mock_tmdb -- 0.0.0.0:9099
# in .env: TMDB_BASE_URL=http://host.docker.internal:9099
#          RUST_LOG=info,sqlx::query=debug   (for the write count)
ADMIN_PASSWORD=... ORIGIN=$PUBLIC_ORIGIN CONTAINER=<container name> scripts/loadtest.sh
```

On shutdown the app logs exact peak RSS and CPU time as `process totals`.

Last measured (amd64 container at the compose limits, 0.5 CPU and 64 MB,
mock TMDB with 120 ms latency):

| Measure | Result | Target |
| --- | --- | --- |
| Idle RSS | 9.4 MB | under 25 MB |
| Peak RSS during the load test (30 Argon2 logins queued) | 32.0 MiB (highest of three runs) | under 50 MB |
| Image size | 33.5 MB unpacked, 14.2 MB compressed | under 50 MB |
| Warm search | 2.7 ms round trip | under 10 ms |
| Cold search | 136 ms (120 ms of it is the mock) | under 800 ms |
| Database writes in 200 requests | 70: 30 requests, 30 events, 10 admin sessions; none from reads | logins, requests and status changes only |
