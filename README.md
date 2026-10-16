# MediaQueue

A small self-hosted request portal. People you give an access code to can
search TMDB and request movies and series. You work through the queue on an
admin page and can get a push notification for every new request.

One Rust binary (Axum, SQLite) with the frontend embedded, shipped as a
distroless container that runs in 64 MB. Design notes are in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and the security review is in
[docs/SECURITY.md](docs/SECURITY.md).

## Quickstart

1. Clone this repository on your server (or copy just `compose.yaml` and
   `.env.example`) and copy the example to `.env`.
2. Generate the access-code key and paste it as `CODE_PEPPER`:
   `openssl rand -base64 48`
3. Hash your admin password and paste it as `ADMIN_PASSWORD_HASH`, keeping the
   single quotes (Compose expands `$` otherwise):
   `docker run --rm -it ghcr.io/askjellevik/mediaqueue:latest hash-password`
4. Create a TMDB "API Read Access Token" (themoviedb.org > Settings > API)
   and set it as `TMDB_TOKEN`.
5. Set `PUBLIC_ORIGIN` to the exact address people will open, for example
   `https://requests.example.com`.
6. Create the data directory for the container's user, then start it:
   `mkdir -p data && sudo chown 65532:65532 data && docker compose up -d`
7. Add a member. A generated code is printed once; to pick one yourself add
   `--code "movie night"` (3 to 64 characters; short codes are easy to guess).
   `docker compose exec portal /app/mediaqueue add-member --name Alice`
8. Open `/admin`, sign in, and manage members, limits and requests there.
   The interface is in Norwegian (bokmål).

## Configuration

Everything is set in `.env`; see the
[configuration table](docs/ARCHITECTURE.md#configuration) for every variable.
Rate limits are built in. Members have no daily request limit unless you set
one on the Brukere tab. `NTFY_URL` and `NTFY_TOPIC` turn on push
notifications (use a long random topic on public ntfy.sh).

## Deploy and update

GitHub Actions builds the arm64 image on pushes that change code, monthly
(for base image fixes) and on demand, and publishes it to
`ghcr.io/askjellevik/mediaqueue` as `latest` and `sha-<commit>`.

Update with `docker compose pull && docker compose up -d`. To roll back,
set `image:` in `compose.yaml` to a `sha-<commit>` tag. If the package is
private, run `docker login ghcr.io` once with a token that has
`read:packages`. Migrations run at startup. When an update changes
`compose.yaml` (for example the healthcheck path), deploy both together.

To build by hand instead (for example with `--build-arg FEATURES=cf-access`),
run `docker buildx build --platform linux/arm64 -t mediaqueue:latest --load .`,
load it on the server and point `image:` at `mediaqueue:latest`.

## Reverse proxy

The container listens on `127.0.0.1:8088` only. With Caddy in front:

```text
requests.example.com {
    reverse_proxy 127.0.0.1:8088 {
        header_up X-Real-IP {client_ip}
    }
}
```

The app uses `X-Real-IP` for its per-visitor lockouts and rate limits, and
trusts it only from `TRUSTED_PROXY_CIDRS` (loopback and Docker's bridge range
by default). It must carry the real visitor address. If the site sits behind
Cloudflare, Caddy would otherwise see only Cloudflare's addresses, and every
visitor would share one bucket. Tell Caddy to trust Cloudflare and read its
header in the global options block (Caddy 2.7 or newer):

```text
{
    servers {
        trusted_proxies static <ranges from https://www.cloudflare.com/ips/>
        client_ip_headers CF-Connecting-IP
    }
}
```

## Backups and restore

Upgrading to the release that lets the admin delete members rebuilds the
`requests` table on first start. Take a backup first (below).

A snapshot is written to `data/backups/` daily and the newest 14 are kept.
Take one now with `docker compose exec portal /app/mediaqueue backup`, and
copy the folder off the machine. To restore, stop the container, replace
`data/db.sqlite3` with a backup (delete `db.sqlite3-wal` and `-shm`), run
`chown 65532:65532` on it and start again. Keep the same `CODE_PEPPER` (back
up `.env` too), or every access code stops working.

## Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| Login or any button fails with 403 | `PUBLIC_ORIGIN` must equal the address in the browser exactly, including the port. The log line "request rejected by the csrf guard" shows what arrived. |
| `docker compose pull` says `denied` | Run `docker login ghcr.io`, or make the package public. |
| Container exits with `configuration error` | The message names the variable; check the quotes around `ADMIN_PASSWORD_HASH`. |
| `unable to open database file` | `data/` must be owned by UID 65532. |
| Everyone is locked out together | The app sees one IP for all visitors; see Reverse proxy. |
| Lost access code | Brukere tab, **Lag ny kode**. |

## Development

```sh
cargo test                       # unit and integration tests
cargo clippy --all-targets && cargo fmt --check
cargo test --features cf-access  # also the Cloudflare Access tests
cargo audit && cargo deny check
```

The `web/` directory is embedded in the binary, so changes there need a
rebuild.

**Styles.** The CSS is Tailwind v4 plus a hand-written `styles/base.css`,
compiled into `web/css/app.css` (committed, so a normal build needs no
Tailwind). After changing `styles/` or a class name in `web/`, run
`scripts/build-css.sh`; it needs the Tailwind standalone CLI (see the
script's header). Write class names as whole strings in the JavaScript, or
the build cannot see them. SQL is checked at compile time against `.sqlx/`; after changing a
query run `cargo sqlx prepare -- --all-targets --all-features` against a
migrated scratch database (`DATABASE_URL=sqlite://$PWD/dev.sqlite3`, then
`sqlx database create && sqlx migrate run`).

**Browser tests.** `tests/browser.rs` drives headless Chrome (the gate under
the real CSP, search races, the keyboard flow, the admin page) and is ignored
by default: `CHROME=/usr/bin/google-chrome cargo test --test browser --
--ignored`. Without Chrome, `node --check web/js/*.js` catches syntax errors.

**Load test.** `scripts/loadtest.sh` sends 200 mixed requests and reports
latency, peak memory and database writes. Run it against the mock TMDB
(`cargo run --release --example mock_tmdb -- 0.0.0.0:9099`, with
`TMDB_BASE_URL` pointing at it), never the real one.
