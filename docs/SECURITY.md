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
| Rotate, revoke and delete remove the member's sessions at once; revoke can be undone with restore, delete cannot and needs a revoke first | `db::rotate_member_code`, `db::revoke_member`, `db::restore_member`, `db::delete_member` | `admin::member_lifecycle_create_devices_rotate_revoke`, `revoke_can_be_undone`, `deleting_a_revoked_member_keeps_their_requests_under_their_name` |
| Wrong, rotated, revoked and deleted codes: same status and body, HMAC and lookup always run | `routes::public::post_auth_code` | `auth_flow::wrong_rotated_and_revoked_codes_return_identical_response`, `admin::deleting_a_revoked_member_keeps_their_requests_under_their_name` |

### Brute force and sharing

| Requirement | Implemented | Tested |
| --- | --- | --- |
| 5 failures per 15 min per IP, then lockouts of 15 min, 1 h, 24 h | `ratelimit::FailureTracker` | `rate_limit::sixth_wrong_code_is_locked_out_with_growing_backoff` |
| IPv6 keyed by /64 | `clientip::bucket_key` | `rate_limit::ipv6_clients_in_one_64_share_a_lockout`, `ipv6_clients_in_one_64_share_an_api_bucket` |
| Global throttle: 1 s delay above 60 failures an hour, 3 s above 300; correct code still logs in | `ratelimit::GlobalThrottle`, `LoginLimiter::before_attempt` | `rate_limit::global_throttle_delays_attempts_but_correct_code_still_logs_in` |
| Delayed attempts hold one of 32 permits; none free gives 429 with `Retry-After` | `ratelimit::LoginLimiter` | `rate_limit::full_or_closed_delay_semaphore_returns_429_never_a_bypass` |
| Throttle on or off: one warning and one ntfy alert, at most one an hour | `ratelimit::LoginLimiter::alert` | `throttle_alert::throttle_turning_on_logs_a_warning_and_sends_exactly_one_alert` |
| At most 10 sessions per member (`MAX_SESSIONS_PER_MEMBER`, a cap on rows, not an anti-sharing control): a new login evicts the oldest, logged | `db::create_user_session`, `post_auth_code` | `auth_flow::login_over_the_device_cap_evicts_oldest_session` |
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

### The worker's internal listener

| Requirement | Implemented | Tested |
| --- | --- | --- |
| A separate listener, not published to the host or routed through Caddy, with its own middleware: no CSRF guard, no per-IP rate limit (Docker clients would share one address), no Cloudflare Access, no browser headers; a 256 KiB body limit, 8 calls in flight, `no-store`, catch-panic, request log (the worker's routine polling, meaning successful progress and settings calls and a claim with nothing to hand out, logs at `debug`; failures, claims that hand out a job and reports stay at `info`) | `lib::internal_router`, `main::serve_async` | `worker::the_internal_listener_has_no_csrf_guard_and_no_browser_headers`, `the_internal_listener_is_not_per_ip_rate_limited` (with the public port as a control), `internal_log::routine_polling_is_not_logged_at_info_and_nothing_secret_is` |
| `/internal` does not exist on the public port | the public router has no such route | `worker::internal_paths_do_not_exist_on_the_public_port` |
| Bearer token checked first on every route (a bad body is 401, not 400), by comparing SHA-256 digests in constant time, so the compared length is fixed; no configured token means everyone is refused | `auth::RequireWorker`, `auth::constant_time_eq` | `worker::every_internal_route_is_401_without_the_right_token`, `without_a_configured_token_the_internal_routes_refuse_everyone`, `auth` unit tests |
| `INTERNAL_TOKEN` at least 32 bytes, never logged; set without `INTERNAL_LISTEN` is a startup error (a misspelt address must not mean no listener) | `config::internal_listener`, redacted `Debug` | `config` unit tests |
| The portal refuses to start if `RD_TOKEN` or `RD_TOKEN_FILE` is set, so the Real-Debrid token never reaches it through `.env` | `config` | `config` unit tests |
| The worker sees ids, title, year and the Infuse PRO flag, never the member's note or name | `routes::internal::ClaimView` | `worker::claim_hands_out_only_the_allowed_fields_and_only_while_enabled` |
| Every report and progress call must carry the job's current `claim_id`; one guarded `UPDATE` makes replayed, forged, reclaimed or overridden calls match no row (409 `stale_claim`, naming the stored torrent so the worker keeps it) | `db::report_fetch_job`, `db::progress_fetch_job`, `error::AppError::StaleClaim` | `worker::a_good_report_applies_once_and_the_old_claim_is_then_stale`, `retry_and_cancel_make_the_running_workers_calls_stale` |
| A `fetching` job with no report or progress call for 45 minutes goes back to `queued`, or fails with reason `max_attempts` once its attempts are used; a progress call refreshes the clock | `db::claim_fetch_job`, `db::RECLAIM_AFTER_SECS` | `worker::a_stale_job_is_claimable_again_with_its_torrent_and_a_new_claim`, `a_progress_call_at_44_minutes_keeps_the_job_from_being_reclaimed_at_50` |
| A human pick's hash cannot be changed by the worker (400 `picked_locked`, after the stale check) | `db::report_fetch_job`, `db::progress_fetch_job` | `worker::a_picked_job_keeps_its_hash_against_progress_and_report` |
| The admin settling the request closes its open job, so a worker still running it is rejected as stale; `done` never overwrites a request that is no longer pending | `db::set_request_status`, `db::report_fetch_job` | `worker::refusing_or_accepting_the_request_makes_the_workers_calls_stale`, `done_leaves_a_request_that_is_no_longer_pending_alone` |
| Worker-supplied text is bounded and cleaned: infohashes exactly 40 hex characters, flat candidates (20 entries, 64 KiB, control characters stripped), a fixed list of reasons, `error` at most 500 characters; the admin UI must render it with `textContent`; logs carry the job id and state only, never candidates, `claim_id`, torrent ids or the token; the auto walk logs one line per tried candidate with the job id, its position, score, size in GB and outcome, and no hash or name; an auto job's final report always carries its shortlist, which the same bounds cover | `routes::internal`, `worker::fetch::log_candidate` | `worker::a_report_with_bad_input_is_400_and_leaves_the_run_alone`, `fetch` unit test `a_tried_candidate_logs_job_position_score_size_and_outcome_but_no_hash_or_name`, `worker_rd::a_done_auto_job_keeps_the_shortlist_the_top_and_the_chosen_hash`, `a_failed_auto_job_keeps_the_shortlist_and_the_last_hash_it_tried`, `an_already_had_auto_job_keeps_the_shortlist_too`, `the_biggest_auto_report_stays_inside_the_portals_limits`, `a_done_report_needs_a_torrent_unless_the_title_was_already_had`, `routes::internal` unit tests, `internal_log::routine_polling_is_not_logged_at_info_and_nothing_secret_is` |
| The worker can raise an alert only by naming one of four fixed kinds; the text, topic and ntfy token stay in the portal, every alert is high priority with the `warning` tag, and one per kind per 30 minutes goes out, so a looping worker cannot flood the phone; the log carries the kind only | `routes::internal::post_alert`, `notify::AlertThrottle` | `worker_alerts::each_kind_sends_one_high_priority_warning_with_portal_written_text`, `a_second_alert_of_the_same_kind_within_thirty_minutes_is_held_back`, `bad_bodies_and_missing_tokens_are_refused_and_send_nothing` |
| The Real-Debrid token is read from `RD_TOKEN_FILE` only (`RD_TOKEN` is refused), trimmed and length-checked, and a file other users can read is refused; it is mounted `:ro` into the worker service only, owned by 65532 with mode 0400 (`./secrets/` is git-ignored), held in a redacted `Secret`, and never reaches the portal, a job row, a report or the logs | `worker::config::read_token_file`, `worker::rd::Secret`, `compose.yaml` (worker block) | `config` unit tests `a_real_debrid_token_in_the_environment_is_refused`, `a_token_file_others_can_read_is_refused`, `debug_shows_no_rd_token`; `compose::worker_mounts_the_token_file_read_only_and_the_state_directory`, `portal_gets_neither_the_token_nor_the_state_directory`, `gitignore_keeps_the_token_and_the_state_out_of_the_repository`; `worker_rd::neither_the_token_nor_a_response_body_reaches_the_logs_of_a_full_run_at_debug`, `_at_trace` |
| Local caps and circuit breaker: `WORKER_MAX_ADDS_PER_DAY` (default 5), `WORKER_MAX_UNCACHED_PER_DAY` (default 2) and `WORKER_MAX_REFUSALS_PER_DAY` (default 20) apply only to torrents the worker chose itself (auto jobs); an add a human picked is not counted against them and not stopped by them, and is only counted in an adds-today statistic that is logged; an add Real-Debrid definitively refuses (classified by `error_code`, e.g. 35) is refunded and counted as a refusal, while network errors, 5xx and unknown outcomes are never refunded; the counters are kept per UTC day in `counters.json` (version 2, version 1 files still load; mode 0600, written to a temp file and renamed, in the `./worker-state` volume) and persisted before the add; a missing file is zeros, an unreadable, corrupt, unknown-version or future-dated one counts as caps reached and raises an alert; Real-Debrid's rate-limit and permission errors open a breaker that blocks every RD call and claim, and the per-minute request budget and the kill switch apply to every job | `worker::state`, `worker::rd`, `worker::fetch`, `worker::run` | `state` unit tests `adds_are_persisted_and_capped`, `a_picked_add_is_not_counted_and_not_blocked_at_the_cap`, `a_refused_add_is_refunded_and_counted_as_a_refusal`, `the_refusals_cap_idles_auto_jobs_only`, `a_file_from_the_previous_version_still_loads`, `corrupt_file_fails_closed_and_is_left_alone`, `future_date_fails_closed_at_open_and_later`, `rate_limit_breaker_doubles_and_caps_at_six_hours`; `worker_rd::a_human_pick_is_neither_counted_nor_blocked_at_the_daily_cap`, `a_refused_add_ends_picked_failed_and_nothing_is_owned`, `a_5xx_or_an_unknown_answer_to_an_add_is_not_refunded`, `an_open_breaker_claims_nothing`, `a_rate_limit_on_the_poll_opens_the_breaker_and_queues_the_deletion`, `an_untrusted_counters_file_raises_its_alert_once_and_claims_nothing` |
| The worker deletes a torrent only if its id is in its own `owned` list in `counters.json`, written when an add returned an id that was not in the library snapshot, before the portal is told; an id from the portal (`rd_torrent_id`, `remove_rd_torrent_id`) is never trusted for deletion, and a removal of an id that is not owned, or is owned for another hash, is cleared without a call; a torrent that cannot be deleted at once is queued and retried | `worker::state` (`own`, `owned_hash`, `pending_delete_*`), `worker::fetch::delete_owned` | `worker_rd::a_torrent_id_from_the_portal_that_is_not_owned_is_never_deleted`, `a_removal_of_a_torrent_not_owned_is_cleared_without_a_delete`, `a_removal_naming_a_torrent_owned_for_another_title_deletes_nothing`, `an_owned_removal_is_deleted_and_cleared`, `a_removal_that_cannot_be_deleted_now_is_queued_and_still_cleared`; `state` unit test `owned_ids_are_pruned_by_age_and_count` |
| Only a validated 40-hex hash reaches Real-Debrid, as a bare `magnet:?xt=urn:btih:<hash>` with no name or trackers from the source; a malformed hash fails the job before an add is counted | `worker::rd::add_magnet` | `rd` unit tests `add_magnet_posts_a_bare_magnet_and_returns_the_id`, `add_magnet_refuses_anything_but_forty_hex_characters_without_a_request`; `worker_rd::a_malformed_hash_fails_before_any_add_is_counted` |
| File rules on the torrent Real-Debrid lists, never the source's claim: an executable or script anywhere (also as the last extension of `x.mkv.exe`) rejects the torrent, so does an archive with no main video; samples and small extras are ignored; two main videos of similar size are ambiguous; only the one chosen file is ever selected, never `all` | `worker::files` (`choose`, `verify`) | `files` unit tests `executables_and_scripts_anywhere_reject_the_torrent`, `a_safe_double_extension_is_not_mistaken_for_an_executable`, `archives_without_a_video_are_unsafe_but_small_ones_are_just_no_video`, `a_sample_is_ignored_in_any_case_and_any_directory`, `two_parts_of_similar_size_are_ambiguous`; `worker_rd::a_torrent_with_an_executable_is_deleted_and_the_pick_fails` |
| Size ceilings differ by profile: a free request keeps the 40 GB shortlist filter, the 15 GB auto ceiling (`WORKER_AUTO_MAX_GB`) and the 30 GB check on Real-Debrid's own file size (`WORKER_RD_MAX_GB`); an Infuse PRO request gets `WORKER_PRO_MAX_GB` (default 100, at most 500) for all three, because a PRO release is streamed, not downloaded. The check on Real-Debrid's list still decides, never the source's size claim, and a PRO auto job has no minimum score or badge but still needs 3 seeders, a known size, a name not cut short and no foreign-language penalty; a free job can only ever take a hash from the free-eligible list | `worker::rules::passes_hard_filters`, `worker::score::auto_eligible`, `worker::fetch` (`eligible_hashes`, `Env::picked_terms`, `Env::auto_terms`), `worker::config` | `rules` unit tests `a_pro_job_may_have_a_far_bigger_file_than_a_free_one`; `score` unit tests `a_pro_candidate_may_be_far_bigger_than_the_free_ceiling_but_not_unsized`, `a_pro_job_skips_few_seeders_foreign_truncated_unsized_and_oversized`, `a_big_or_unsized_free_candidate_is_skipped`; `fetch` unit tests `a_free_job_never_gets_a_hash_without_a_yes_badge_even_if_the_plan_listed_it`, `a_pro_job_gets_its_hashes_in_pro_order_up_to_its_own_cap`; `config` unit test `the_pro_size_ceiling_defaults_to_100_gb_and_stays_in_range`; `worker_rd::a_pro_pick_of_forty_gib_is_taken_where_a_free_pick_is_refused`, `a_pro_job_takes_the_biggest_release_under_its_own_ceiling_first` |
| An auto job is satisfied by the library only on its own profile's terms: a downloaded torrent whose name carries the job's title and the same year (not a pack, extras, disc or cam copy, and for PRO not an upscale) counts as `already_had` when, for a free request, its release passes the free profile (badge `yes`), and, for a PRO request, it is at least `WORKER_PRO_HAVE_MIN_HEIGHT` (1080) tall and `WORKER_PRO_HAVE_MIN_GB` (8) big. A TrueHD or DTS remux therefore never stands in for a free copy and a small AAC encode never for a PRO one, so a free copy and a PRO copy of one film can coexist. It reads the listing the run already fetches (no extra Real-Debrid call), adds nothing, counts nothing against the caps, logs one line with the job id and the reason, and never applies to a human's pick | `worker::fetch::already_had`, `worker::rules::library_release`, `worker::config` | `rules` unit test `a_library_name_must_carry_the_title_and_the_year`; `config` unit test `the_pro_library_thresholds_default_and_can_be_overridden`; `worker_rd::an_auto_free_job_is_already_had_when_the_library_holds_a_free_safe_copy`, `an_auto_pro_job_is_already_had_when_the_library_holds_a_big_enough_copy`, `a_human_pick_is_added_even_when_the_library_holds_a_copy`, `the_pro_library_thresholds_are_a_height_and_a_size`, `a_downloaded_pro_remux_does_not_satisfy_a_free_request`, `a_small_free_safe_encode_does_not_satisfy_a_pro_request`, `a_free_copy_and_a_pro_copy_of_the_same_film_can_coexist` |
| Automatic jobs exist only for movie requests whose first requester the admin opted in (`members.auto_fetch`, default off), created after the moment the mode became `auto`, at most `AUTO_FETCH_PER_MEMBER_DAY` per member and UTC day, and only while the worker is on in `auto` mode | `db::sweep_auto_jobs`, `tasks::spawn_auto_sweep`, `routes::admin::patch_member` | `worker::the_sweep_queues_only_requests_that_qualify`, `nothing_is_swept_unless_the_worker_is_on_in_auto_mode`, `a_member_gets_at_most_the_daily_number_of_automatic_jobs`, `switching_to_auto_stamps_the_cut_off_and_leaves_older_requests_to_the_button` |

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
  unique (case-insensitively) to make that sound. Renaming a member rewrites
  the name on their events in the same transaction, so their list is kept.
- **Deleting a member keeps their requests.** `requests.requested_by`
  becomes `NULL` and `requested_by_name` holds the name; their events are
  relabelled `<name> (deleted)`. Names ending in ` (deleted)` are refused
  for new or renamed members, so a new member with the same name cannot
  inherit the old history, and SQLite reusing a member id cannot either.
  The name stays as plain text in the archive and CSV.
- **Same-status PATCH is a no-op.** It returns `changed: false` and writes
  no event, so a double click never adds noise to the archive. The one
  exception is `refused` with a different reason (or none, which clears it):
  that updates the reason and logs one event. The reason is cleaned like a
  member's note (control characters stripped, at most 300 characters) and
  reaches the page through `textContent` only.
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
