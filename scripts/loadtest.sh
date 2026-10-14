#!/usr/bin/env bash
# Fires 200 mixed requests at a running portal: searches (warm and cold),
# /api/me, new requests, and admin logins with right and wrong passwords
# (each one a real Argon2id hash). Reports status codes, p50/p95 latency,
# peak RSS sampled during the run and the database writes it caused.
#
# Point TMDB_BASE_URL at the mock first, never at the real TMDB:
#   cargo run --release --example mock_tmdb -- 0.0.0.0:9099
#
# Usage (the defaults match a local `docker compose up`):
#   ADMIN_PASSWORD=... ORIGIN=https://requests.example.com \
#   CONTAINER=mqload-portal-1 scripts/loadtest.sh
#
# Needs bash, curl, awk, sort, shuf and xargs. For the write count the app
# must log SQL statements: RUST_LOG=info,sqlx::query=debug.
set -euo pipefail

BASE=${BASE:-http://127.0.0.1:8088}
ORIGIN=${ORIGIN:-https://requests.example.test} # must equal PUBLIC_ORIGIN
ADMIN_USER=${ADMIN_USER:-admin}
ADMIN_PASSWORD=${ADMIN_PASSWORD:?set ADMIN_PASSWORD}
MEMBERS=${MEMBERS:-10}
CONCURRENCY=${CONCURRENCY:-16}
CONTAINER=${CONTAINER:-}
DOCKER=${DOCKER:-docker}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
export BASE ORIGIN WORK ADMIN_USER ADMIN_PASSWORD

# Every request claims its own client IP, as Caddy would send for real
# visitors; the app trusts X-Real-IP from the Docker bridge. One IP for
# everything would only measure the per-IP limiter.
random_ip() { echo "10.$((RANDOM % 250)).$((RANDOM % 250)).$((RANDOM % 250 + 1))"; }
export -f random_ip

json_post() { # path body [cookie]
  curl -s -i -X POST "$BASE$1" \
    -H "Origin: $ORIGIN" -H 'X-Requested-With: mp' -H 'Content-Type: application/json' \
    -H "X-Real-IP: $(random_ip)" ${3:+-H "Cookie: $3"} --data "$2"
}

cookie_of() { grep -i -m1 '^set-cookie:' | sed -E 's/^[Ss]et-[Cc]ookie: ([^;]+);.*/\1/' | tr -d '\r'; }

# ---------------------------------------------------------------------
# Setup: admin session, members, member sessions
# ---------------------------------------------------------------------

ADMIN_COOKIE=$(json_post /api/admin/login "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASSWORD\"}" | cookie_of)
[[ -n $ADMIN_COOKIE ]] || { echo "admin login failed" >&2; exit 1; }
export ADMIN_COOKIE

run_id=$RANDOM
for i in $(seq 1 "$MEMBERS"); do
  code=$(json_post /api/admin/members "{\"name\":\"load-$run_id-$i\",\"daily_limit\":100}" "$ADMIN_COOKIE" |
    sed -nE 's/.*"code":"([A-Z0-9-]+)".*/\1/p')
  [[ -n $code ]] || { echo "creating member $i failed" >&2; exit 1; }
  json_post /api/auth/code "{\"code\":\"$code\"}" | cookie_of >"$WORK/member-$i"
done

# ---------------------------------------------------------------------
# The 200 requests, shuffled
# ---------------------------------------------------------------------

queries=(dune alien heat jaws rocky fargo gladiator memento inception arrival
  matrix up coco brave cars frozen shrek tenet speed casino)
{
  for n in $(seq 0 79); do echo "search $((n % MEMBERS + 1)) ${queries[$((n % 20))]}"; done
  for n in $(seq 0 49); do echo "me $((n % MEMBERS + 1))"; done
  for n in $(seq 0 29); do echo "request $((n % MEMBERS + 1)) $((run_id * 100 + n + 1))"; done
  for n in $(seq 1 20); do echo "admin_wrong"; done
  for n in $(seq 1 10); do echo "admin_right"; done
  for n in $(seq 1 10); do echo "admin_list"; done
} | shuf >"$WORK/plan"

one() {
  read -r kind member arg <<<"$1"
  local cookie="" fmt="$kind %{http_code} %{time_total}\n"
  [[ -n ${member:-} ]] && cookie=$(cat "$WORK/member-$member")
  local common=(-s -o /dev/null -w "$fmt" -H "X-Real-IP: $(random_ip)" -H 'X-Requested-With: mp')
  local post=(-X POST -H "Origin: $ORIGIN" -H 'Content-Type: application/json')
  case $kind in
    search) curl "${common[@]}" -H "Cookie: $cookie" "$BASE/api/search?q=$arg" ;;
    me) curl "${common[@]}" -H "Cookie: $cookie" "$BASE/api/me" ;;
    request) curl "${common[@]}" "${post[@]}" -H "Cookie: $cookie" \
      --data "{\"tmdb_id\":$arg,\"media_type\":\"movie\"}" "$BASE/api/requests" ;;
    admin_wrong) curl "${common[@]}" "${post[@]}" \
      --data "{\"username\":\"$ADMIN_USER\",\"password\":\"wrong-$RANDOM\"}" "$BASE/api/admin/login" ;;
    admin_right) curl "${common[@]}" "${post[@]}" \
      --data "{\"username\":\"$ADMIN_USER\",\"password\":\"$ADMIN_PASSWORD\"}" "$BASE/api/admin/login" ;;
    admin_list) curl "${common[@]}" -H "Cookie: $ADMIN_COOKIE" "$BASE/api/admin/requests?status=all" ;;
  esac
}
export -f one

# Sample the container's RSS while the load runs.
sample_rss() {
  while :; do
    "$DOCKER" top "$CONTAINER" -o pid,rss 2>/dev/null | awk 'NR > 1 { print $2 }' >>"$WORK/rss"
    sleep 0.2
  done
}
if [[ -n $CONTAINER ]]; then
  sample_rss &
  SAMPLER=$!
fi

since=$(date -u +%Y-%m-%dT%H:%M:%S.%NZ)
started=$(date +%s.%N)
# A failed curl (status 000) is reported below, not fatal.
xargs -P "$CONCURRENCY" -I{} bash -c 'one "$@"' _ {} <"$WORK/plan" >"$WORK/results" || true
elapsed=$(awk -v s="$started" -v e="$(date +%s.%N)" 'BEGIN { printf "%.2f", e - s }')
if [[ -n ${SAMPLER:-} ]]; then kill "$SAMPLER" 2>/dev/null || true; fi

# ---------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------

echo "== $(wc -l <"$WORK/results") requests in ${elapsed}s, concurrency $CONCURRENCY"
echo
echo "== status codes per kind"
awk '{ c[$1 " " $2]++ } END { for (k in c) print "  " k ": " c[k] }' "$WORK/results" | sort

pct() { # file percentile
  sort -n "$1" | awk -v p="$2" '{ a[NR] = $1 } END { i = int((NR - 1) * p / 100) + 1; printf "%.1f", a[i] * 1000 }'
}
echo
echo "== latency in ms (p50 / p95 / max)"
for kind in all search me request admin_right admin_wrong admin_list; do
  if [[ $kind == all ]]; then awk '{ print $3 }' "$WORK/results" >"$WORK/t"
  else awk -v k="$kind" '$1 == k { print $3 }' "$WORK/results" >"$WORK/t"; fi
  [[ -s $WORK/t ]] || continue
  printf "  %-12s %7s / %7s / %7s\n" "$kind" "$(pct "$WORK/t" 50)" "$(pct "$WORK/t" 95)" "$(pct "$WORK/t" 100)"
done

if [[ -n $CONTAINER ]]; then
  echo
  echo "== container"
  if [[ -s $WORK/rss ]]; then
    sort -n "$WORK/rss" | tail -1 | awk '{ printf "  peak sampled RSS: %.1f MiB\n", $1 / 1024 }'
  fi
  logs=$("$DOCKER" logs --since "$since" "$CONTAINER" 2>&1 || true)
  writes=$(grep -cE '"summary":"(INSERT|UPDATE|DELETE)' <<<"$logs" || true)
  echo "  database write statements during the run: $writes"
  grep -oE '"summary":"(INSERT INTO|UPDATE|DELETE FROM) [a-z_]+' <<<"$logs" |
    sed -E 's/"summary":"//' | sort | uniq -c | sed 's/^/   /' || true
  echo "  exact peak RSS and CPU time are logged as \"process totals\" on shutdown:"
  echo "    $DOCKER stop $CONTAINER && $DOCKER logs $CONTAINER 2>&1 | grep 'process totals'"
fi
