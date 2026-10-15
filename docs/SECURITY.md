# Security review

Each security control points to where it is implemented and where it is
tested. Test names are `file::test`. "Unit" means a `#[cfg(test)]` test in
the same source file. For the overall design, see
[ARCHITECTURE.md](ARCHITECTURE.md).

## Authentication and security

### Access codes and user sessions

| Requirement | Implemented | Tested |
| --- | --- | --- |
| Generated codes: 8 characters from the 31-symbol alphabet, shown `ABCD-EFGH` | `auth::generate_code`, `auth::format_code_for_display` | unit `lookalike_characters_never_appear_in_generated_codes`, `display_format_inserts_single_dash` |
| OS random source with rejection sampling, no modulo bias | `auth::generate_code` (`SysRng`, bytes ≥ 248 rejected) | unit `generated_codes_use_every_symbol_about_evenly` |
| Normalise: uppercase, strip spaces and hyphens, hash that | `auth::normalise_code` | `auth_flow::code_normalisation_variants_all_authenticate` |
| Only `HMAC-SHA256(CODE_PEPPER, code)` stored, unique index, constant-time compare with `subtle` | `auth::hmac_code`, `db::find_member_by_code_hash`, `routes::public::post_auth_code` (`auth::constant_time_eq`) | `auth_flow::database_holds_no_raw_code_or_token`, unit `hmac_is_deterministic_and_key_dependent` |
| `CODE_PEPPER` at least 32 bytes | `config` | unit `rejects_short_pepper` |
| Code shown once, never readable again; an admin-chosen code is never echoed back | `routes::admin::post_member`, `post_rotate`; no other route selects `code_hash` | `admin::no_endpoint_returns_a_code_except_create_and_rotate`, `admin::admin_can_choose_a_members_code_on_create_and_rotate` |
| Admin-chosen codes: 3 to 64 characters after normalising, unique, 400 or 409 otherwise. **Short codes are guessable**; what limits the damage is the per-IP lockout (about 5 guesses per IP a day after the first hour), the global throttle with its ntfy alert, per-member revocation, and that a guessed code only allows searching TMDB and filing requests under that member's name. This holds only when the app sees real visitor IPs (see the reverse proxy section of the README). | `auth::check_custom_code`, `routes::admin::chosen_code_hash` | unit `custom_codes_are_normalised_and_length_checked`, `admin::admin_can_choose_a_members_code_on_create_and_rotate` |
| Session: 32 random bytes in the cookie, SHA-256 in the database | `auth::generate_session_token`, `auth::hash_token` | unit `session_token_hash_matches_hash_token`, `auth_flow::database_holds_no_raw_code_or_token` |
| 90-day sliding sessions, touched at most every 10 minutes | `auth::RequireUser` | `auth_flow::me_requires_session_and_reports_remaining_today`; load test: no `UPDATE` in 200 requests |
| Rotate and revoke delete the member's sessions at once | `db::rotate_member_code`, `db::revoke_member` | `admin::member_lifecycle_create_devices_rotate_revoke` |
| Wrong, rotated and revoked codes: same status and body, HMAC and lookup always run | `routes::public::post_auth_code` | `auth_flow::wrong_rotated_and_revoked_codes_return_identical_response` |

### Brute force and sharing

| Requirement | Implemented | Tested |
| --- | --- | --- |
| 5 failures per 15 min per IP, then lockouts of 15 min, 1 h, 24 h | `ratelimit::FailureTracker` | `rate_limit::sixth_wrong_code_is_locked_out_with_growing_backoff` |
| IPv6 keyed by /64 | `clientip::bucket_key` | `rate_limit::ipv6_clients_in_one_64_share_a_lockout`, `ipv6_clients_in_one_64_share_an_api_bucket` |
| Global throttle: 1 s delay above 60 failures an hour, 3 s above 300; correct code still logs in | `ratelimit::GlobalThrottle`, `LoginLimiter::before_attempt` | `rate_limit::global_throttle_delays_attempts_but_correct_code_still_logs_in` |
| Delayed attempts hold one of 32 permits; none free gives 429 with `Retry-After` | `ratelimit::LoginLimiter` | `rate_limit::full_or_closed_delay_semaphore_returns_429_never_a_bypass` |
| Throttle on or off: one warning and one ntfy alert, at most one an hour | `ratelimit::LoginLimiter::alert` | `throttle_alert::throttle_turning_on_logs_a_warning_and_sends_exactly_one_alert` |
| `max_sessions` (default 10 for new members): a new login evicts the oldest, logged | `db::create_user_session`, `post_auth_code` | `auth_flow::fourth_login_evicts_oldest_session_for_max_sessions_three` |
| Admin page shows devices per member, with a kill button per device | `routes::admin::get_members`, `delete_session`, `web/js/admin.js` | `admin::member_lifecycle_create_devices_rotate_revoke`, `browser::admin_queue_archive_and_members_work_end_to_end` |

### Admin login

| Requirement | Implemented | Tested |
| --- | --- | --- |
| `ADMIN_USER` and an Argon2id PHC hash from env; malformed hash refused at startup | `config` | unit `rejects_malformed_admin_password_hash` |
| `hash-password` prompts without echo and prints only the hash | `main::run_hash_password` (`rpassword`) | manual: run under a pty while building the load test |
| Argon2id 19 MiB, t=2, p=1 in `spawn_blocking`, semaphore of 1 | `auth::argon2_params`, `auth::verify_admin_password`, `AppState::admin_login_semaphore` | unit `verify_admin_password_*`; load test peak 32.0 MiB with 30 logins queued |
| Unknown usernames still run a real verification | `auth::admin_password_hash_for`, `dummy_password_hash` | `auth_flow::admin_login_rejects_unknown_username_and_wrong_password_identically`, unit `dummy_hash_is_well_formed_but_never_guessable` |
| 8 h absolute, 2 h idle, new token every login | `auth::RequireAdmin`, `routes::admin::post_admin_login` | `hardening::admin_session_ends_after_two_idle_hours`, `admin_session_ends_after_eight_hours_even_when_active`, `every_admin_login_issues_a_new_token` |
| Same per-IP failure tracking; global throttle above 30 per 15 min | `RateLimits::admin_login` | `rate_limit::admin_login_is_locked_out_per_ip_independently_of_codes`, `admin_throttle_is_separate_from_code_throttle` |
| Optional Cloudflare Access JWT for `/admin` and `/api/admin` (feature and env switch, cached keys) | `cf_access` (feature `cf-access`, `CF_ACCESS_ENABLED`) | `cf_access::*` (run with `--features cf-access`), unit `protects_every_way_to_the_admin_area` |
| A member session on an admin route gets 403, no session gets 401 | `auth::RequireAdmin` (`has_user_session`) | `admin::every_admin_route_is_401_anonymous_and_403_for_members` |

### Cookies and CSRF

| Requirement | Implemented | Tested |
| --- | --- | --- |
| `__Host-mp_user` and `__Host-mp_admin`: `HttpOnly`, `Secure`, `SameSite=Strict`, `Path=/`, no `Domain` | `auth::set_cookie_header` | `auth_flow::login_sets_host_prefixed_secure_cookie`, unit `session_cookie_has_required_flags` |
| An `http://` `PUBLIC_ORIGIN` drops the prefix and `Secure` (local development); startup warns if the host is not local. An `https://` origin never does. | `auth::cookie_name`, `config`, `main::warn_if_insecure_origin` | `auth_flow::dev_insecure_cookie_drops_prefix_and_secure`, unit `http_origin_enables_insecure_cookies`, `https_origin_keeps_cookies_secure` |
| State-changing requests need JSON, `X-Requested-With: mp` and a matching `Origin`, else 403 | `middleware::csrf_guard` | `auth_flow::missing_x_requested_with_returns_403`, `wrong_origin_returns_403`, `non_json_content_type_returns_403`, `admin::admin_routes_still_need_the_csrf_headers` |
| A rejected request is logged with the failing check and the received `Origin`; the response does not say which | `middleware::csrf_guard` | `csrf_log::every_failing_combination_is_403_and_logged_with_its_reason` |

### Live update streams

| Requirement | Implemented | Tested |
| --- | --- | --- |
| Streams need the same session as the matching API: 401 without one, 403 for a member on the admin stream | `routes::public::get_events`, `routes::admin::get_events` | `events::streams_need_the_right_session` |
| Ticks carry no data, so nothing leaks to a reader | `events::tick_event` | `events::status_change_ticks_members_and_admin`, `new_and_duplicate_requests_tick_only_the_admin` |
| Every 25 seconds a stream re-checks its session, read-only, and ends if it was revoked, rotated, logged out or expired | `events::Hub::open`, `db::session_alive` | `events::stream_ends_when_the_member_is_revoked`, `stream_ends_when_the_admin_logs_out_elsewhere` |
| At most 200 streams; a dropped stream frees its slot; shutdown ends them all | `events::Hub` | `events::permits_are_released_when_a_stream_drops_and_the_cap_gives_503`, `shutdown_ends_every_stream` |
| Open streams are not cut off by the request timeout and do not use up the concurrency limit | `middleware::timeout_layer` covers only the response head | `events::stream_outlives_the_request_timeout`, `open_streams_do_not_use_up_the_request_concurrency` |

### Headers and input handling

| Requirement | Implemented | Tested |
| --- | --- | --- |
| A strict CSP (`default-src 'none'`, scripts and styles from self only), no inline scripts or styles | `middleware::apply_security_headers`, `web/` | `hardening::every_response_carries_the_security_headers`, `frontend::index_references_assets_by_current_version_and_has_no_inline_code`, `browser::gate_renders_under_the_real_csp_without_console_errors` |
| `nosniff`, `no-referrer`, `Permissions-Policy`, `Cross-Origin-Opener-Policy: same-origin` | `middleware::apply_security_headers` | `hardening::every_response_carries_the_security_headers` |
| Typed input: `tmdb_id` u32 > 0, `media_type` enum, note ≤ 200 with control characters stripped, `deny_unknown_fields` | `routes::public::CreateRequestBody`, `clean_note` | `requests::malformed_bodies_are_rejected_without_touching_tmdb`, `note_is_stored_cleaned` |
| SQL only through bound parameters | `db` (every query is `query!`, `query_as!` or a bound `query`) | compile-time checked; `admin::queue_rows_carry_requester_imdb_and_dmm_link_and_filter_by_text` (LIKE wildcards taken literally) |
| TMDB text rendered with `textContent`, never `innerHTML` | `web/js/ui.js::el` | `frontend::scripts_never_write_html` |
| Never log cookies, codes, passwords or full bodies | every `tracing` call | `request_log::requests_are_logged_without_secrets`, `throttle_alert::*` |

### Client IP, resilience and exposure

| Requirement | Implemented | Tested |
| --- | --- | --- |
| `X-Real-IP` only from peers in `TRUSTED_PROXY_CIDRS`; `X-Forwarded-For` ignored | `clientip` | unit tests in `clientip`, `rate_limit::spoofed_x_real_ip_from_untrusted_peer_cannot_dodge_the_limit` |
| 10 s timeout, concurrency limit 64, 16 KB body limit, catch-panic | `middleware`, `lib::build_router` | `hardening::bodies_over_16_kb_are_refused`, `panic_safety::panicking_route_returns_500_and_server_keeps_serving`; the timeout and concurrency limit are configured but have no test (one would wait 10 s) |
| Structured request log without sensitive fields | `middleware::request_log` | `request_log::requests_are_logged_without_secrets` |
| Published only on `127.0.0.1:8088` | `compose.yaml` | checked in the container run |
| Distroless, UID 65532, read-only root, all caps dropped, `no-new-privileges`, only `/data` writable | `Dockerfile`, `compose.yaml` | container run: `user=65532:65532 ro=true caps=[ALL]`, healthy |
| No secrets in the image or repo; `.env` ignored | `.gitignore`, `.dockerignore`, `.env.example` | review |

## Rate limiting and TMDB caching

| Requirement | Implemented | Tested |
| --- | --- | --- |
| Search per member: burst 10, refill 5/s, 429 with `Retry-After` | `RateLimits::check_search`, `routes::public::get_search` | `search::search_is_rate_limited_per_member` |
| Create request: 10 a minute per member, plus a daily limit only if the admin set one; 429 with `remaining_today` when a daily limit exists; counted from `request_events` | `routes::public::Quota`, `db::quota_usage`, re-checked in `db::create_request` | `requests::eleventh_request_in_a_day_is_429_and_writes_nothing`, `fourth_request_in_a_minute_is_429_until_the_oldest_ages_out`, `member_daily_limit_caps_requests`, `member_without_a_daily_limit_is_never_daily_capped`, `concurrent_submits_never_overshoot_the_minute_cap` |
| Any API call: 120 a minute per IP, before body parsing | `middleware::api_rate_limit` | `rate_limit::api_limit_bursts_refills_and_sets_retry_after`, `api_limit_runs_before_csrf_and_body_parsing` |
| TMDB upstream: 20/s globally, then stale or 503 | `tmdb::Tmdb::get_json` (`limiter`) | `search::global_tmdb_limit_answers_503_without_calling_upstream` |
| governor keyed limiters, `retain_recent` and `shrink_to_fit` every minute, 10,000-key cap failing closed | `ratelimit::KeyedLimiter`, `tasks::spawn_rate_limit_maintenance` | `ratelimit` unit tests (100,000 keys), `rate_limit::maintenance_evicts_idle_api_keys` |
| Failure tracker in `std::sync::Mutex<HashMap>`, never held across `.await`, same cap and cleanup | `ratelimit::FailureTracker` | `ratelimit` unit tests |
| Every 429 carries `Retry-After` | `error::AppError::into_response` | `rate_limit::assert_429`, `requests::eleventh_*`, `search::search_is_rate_limited_per_member` |
| `/search/multi`, people dropped, 8 results, slim payload | `tmdb::slim_results` | `search::people_are_dropped_and_payload_is_slim_and_capped_at_eight` |
| Injectable clock for every freshness decision; moka TTL only frees memory | `clock::Clock`, `tmdb::Tmdb::cached` | every `search::*` cache test uses the fake clock |
| Fresh, stale-usable (refresh, 30 s unhealthy), gone (503) | `tmdb::Tmdb::cached` | `search::repeat_search_in_the_same_epoch_is_served_fresh`, `successful_refresh_replaces_the_stale_entry_with_one_call`, `failed_refresh_serves_stale_and_backs_off_for_30_seconds`, `refresh_that_times_out_serves_stale`, `entry_older_than_an_hour_is_never_served` |
| Single flight via `try_get_with`; failed loads not cached | `tmdb::Tmdb::cached` | `search::twenty_concurrent_identical_searches_make_one_upstream_call` |
| Other 4xx never answered from stale data; the entry is removed | `tmdb::Tmdb::cached` | `search::detail_404_is_not_found_even_with_a_stale_entry_and_removes_it` |
| Detail cache: same states, daily epoch, 7-day limit | `tmdb::Tmdb::detail` | `search::stale_detail_is_served_while_tmdb_is_down_for_up_to_a_week` |
| 3 s connect and total timeouts, one retry on connection errors only | `state::AppState::with_clock`, `tmdb::Tmdb::get_json` | `search::refresh_that_times_out_serves_stale`; the connect-error retry is reviewed but not tested |
| One `SELECT ... IN (...)` for all result states | `db::request_states` (`json_each`) | `search_state_query::state_annotation_is_one_select_regardless_of_result_count` |
| Posters from `image.tmdb.org` at `w92` and `w185`, never proxied | `web/js/ui.js::posterSmall`, `posterLarge` | CSP `img-src`; browser tests |
| IMDb ID only on submit, never during search | `routes::public::post_request` | `requests::two_members_requesting_one_title_make_one_row_and_two_events` |
| TMDB attribution in the footer | `web/js/ui.js::footer`, `web/img/tmdb.svg` | `browser::typing_race_and_keyboard_flow_work_end_to_end` (renders), manual |

## Design notes

- **Duplicate events have no member id.** `request_events` only stores
  `actor` (the member name). So the per-minute quota and "My requests"
  match a member's duplicate events by name, and member names are kept
  unique (case-insensitively) to make that sound. Renaming a member can
  hide older duplicate requests from their list.
- **Same-status PATCH is a no-op.** It returns `changed: false` and writes
  no event, so a double click never adds noise to the archive.
- **TV details use one call.** `/tv/{id}?append_to_response=external_ids`
  returns the same IMDb ID as `/tv/{id}/external_ids` in a single request.
- **`GLIBC_TUNABLES` in the Dockerfile.** Without a fixed mmap threshold,
  glibc keeps each 19 MiB Argon2 block in a per-thread arena. Under the
  load test, RSS ratcheted up until the 64 MB container was OOM-killed.
  With the tunable, peak RSS is about 32 MiB and returns to about 13 MB.
  Argon2's cost and the memory limit are unchanged.
- **`rsa` advisory RUSTSEC-2023-0071 is ignored.** It only enters through
  the optional `cf-access` feature, which uses public-key verification only.
  The Marvin attack targets private-key operations.
