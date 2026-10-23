//! The worker's internal API and the admin endpoints that create and steer
//! fetch jobs, against the real app: the internal listener is a second
//! server on its own port, like in production.

mod common;

use std::{collections::HashSet, time::Duration};

use common::{
    EventStream, HttpResponse, TEST_INTERNAL_TOKEN, TestApp, json_post_headers, login_admin_cookie,
};
use serde_json::{Value, json};
use sqlx::SqlitePool;

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SOON: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(700);
const MINUTE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn json_of(response: &HttpResponse) -> Value {
    serde_json::from_str(&response.body).unwrap_or_else(|_| panic!("not json: {}", response.body))
}

/// An admin API call with the CSRF headers a browser would send.
async fn admin_call(
    app: &TestApp,
    cookie: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("Cookie", cookie));
    app.request(method, path, &headers, body).await
}

/// A worker call with the right token.
async fn worker(app: &TestApp, method: &str, path: &str, body: Option<&Value>) -> HttpResponse {
    let auth = format!("Bearer {TEST_INTERNAL_TOKEN}");
    let body = body.map(Value::to_string);
    app.internal_request(
        method,
        path,
        &[
            ("Authorization", auth.as_str()),
            ("Content-Type", "application/json"),
        ],
        body.as_deref(),
    )
    .await
}

async fn alice(app: &TestApp) -> i64 {
    let existing: Option<i64> = sqlx::query_scalar("SELECT id FROM members WHERE name = 'Alice'")
        .fetch_optional(&app.pool)
        .await
        .unwrap();
    match existing {
        Some(id) => id,
        None => {
            common::create_member(&app.pool, &app.config, "Alice")
                .await
                .0
        }
    }
}

/// A request by Alice with a note, like `POST /api/requests` would leave
/// it. `imdb` of `None` is a request without an IMDb ID.
async fn seed_request(
    app: &TestApp,
    tmdb_id: i64,
    status: &str,
    imdb: Option<&str>,
    infuse_pro: bool,
) -> i64 {
    let member = alice(app).await;
    let now = i64::try_from(app.clock.unix_secs()).unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO requests (tmdb_id, media_type, imdb_id, title, year, note, infuse_pro, status,
                               requested_by, created_at, status_changed_at)
         VALUES (?, 'movie', ?, 'Dune', 2021, 'secret note', ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(tmdb_id)
    .bind(imdb)
    .bind(infuse_pro)
    .bind(status)
    .bind(member)
    .bind(now)
    .bind(now)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO request_events (request_id, kind, actor, created_at)
         VALUES (?, 'submitted', 'Alice', ?)",
    )
    .bind(id)
    .bind(now)
    .execute(&app.pool)
    .await
    .unwrap();
    id
}

async fn enable_worker(app: &TestApp, admin: &str) {
    let response = admin_call(
        app,
        admin,
        "PUT",
        "/api/admin/worker",
        Some(r#"{"enabled":true}"#),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
}

/// Creates the job for a request through the admin API; returns its id.
async fn create_job(app: &TestApp, admin: &str, request_id: i64) -> i64 {
    let response = admin_call(
        app,
        admin,
        "POST",
        &format!("/api/admin/requests/{request_id}/fetch"),
        Some("{}"),
    )
    .await;
    assert_eq!(response.status, 201, "{}", response.body);
    json_of(&response)["job"]["id"].as_i64().unwrap()
}

struct Running {
    admin: String,
    request_id: i64,
    job_id: i64,
    claim_id: String,
}

/// An enabled worker, one pending request with a job, claimed.
async fn running(app: &TestApp) -> Running {
    let admin = login_admin_cookie(app).await;
    enable_worker(app, &admin).await;
    let request_id = seed_request(app, 1, "pending", Some("tt0000001"), false).await;
    let job_id = create_job(app, &admin, request_id).await;
    let claim = worker(app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(claim.status, 200, "{}", claim.body);
    let claim = json_of(&claim);
    assert_eq!(claim["id"], job_id);
    Running {
        admin,
        request_id,
        job_id,
        claim_id: claim["claim_id"].as_str().unwrap().to_string(),
    }
}

#[derive(Debug, PartialEq, sqlx::FromRow)]
struct JobRow {
    state: String,
    claim_id: Option<String>,
    rd_torrent_id: Option<String>,
    chosen_infohash: Option<String>,
    reason: Option<String>,
    uncached_adds: i64,
}

async fn job_row(pool: &SqlitePool, id: i64) -> JobRow {
    sqlx::query_as(
        "SELECT state, claim_id, rd_torrent_id, chosen_infohash, reason, uncached_adds
         FROM fetch_jobs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn request_status(pool: &SqlitePool, id: i64) -> String {
    sqlx::query_scalar("SELECT status FROM requests WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn sql(pool: &SqlitePool, statement: &str) {
    sqlx::query(sqlx::AssertSqlSafe(statement.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

async fn count(pool: &SqlitePool, statement: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(statement.to_string()))
        .fetch_one(pool)
        .await
        .unwrap()
}

fn keys(value: &Value) -> HashSet<&str> {
    value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

/// `rd_torrent_id` of an error body: a missing key and `null` both mean
/// "no stored torrent".
fn stored_torrent(body: &Value) -> Option<&str> {
    body.get("rd_torrent_id").and_then(Value::as_str)
}

fn assert_error(response: &HttpResponse, status: u16, code: &str) -> Value {
    assert_eq!(response.status, status, "{}", response.body);
    let body = json_of(response);
    assert_eq!(body["error"], code, "{}", response.body);
    assert!(
        !body["message"].as_str().unwrap_or("").is_empty(),
        "{}",
        response.body
    );
    body
}

// ---------------------------------------------------------------------
// Authentication and the two listeners
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_internal_route_is_401_without_the_right_token() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let routes = [
        ("POST", "/internal/jobs/claim"),
        ("POST", "/internal/jobs/1/progress"),
        ("POST", "/internal/jobs/1/report"),
        ("GET", "/internal/settings"),
        // A path the typed extractor would answer 404 for.
        ("POST", "/internal/jobs/abc/report"),
    ];
    let wrong = format!("Bearer {TEST_INTERNAL_TOKEN}x");
    let basic = format!("Basic {TEST_INTERNAL_TOKEN}");
    let bare = TEST_INTERNAL_TOKEN.to_string();
    let lower = format!("bearer {TEST_INTERNAL_TOKEN}");
    let cookie_only = [("Cookie", admin.as_str())];
    let attempts: [Vec<(&str, &str)>; 7] = [
        vec![],
        vec![("Authorization", wrong.as_str())],
        vec![("Authorization", "Bearer ")],
        vec![("Authorization", basic.as_str())],
        vec![("Authorization", bare.as_str())],
        vec![("Authorization", lower.as_str())],
        cookie_only.to_vec(),
    ];
    for (method, path) in routes {
        for headers in &attempts {
            let mut headers = headers.clone();
            headers.push(("Content-Type", "application/json"));
            // A garbage body must not turn the 401 into a 400.
            let response = app
                .internal_request(method, path, &headers, Some("not json"))
                .await;
            assert_eq!(
                response.status, 401,
                "{method} {path} {headers:?}: {}",
                response.body
            );
            assert_eq!(json_of(&response)["error"], "unauthorized");
        }
    }
    // With the token the same garbage is the usual 400.
    let auth = format!("Bearer {TEST_INTERNAL_TOKEN}");
    let response = app
        .internal_request(
            "POST",
            "/internal/jobs/1/report",
            &[
                ("Authorization", auth.as_str()),
                ("Content-Type", "application/json"),
            ],
            Some("not json"),
        )
        .await;
    assert_eq!(response.status, 400);
}

#[tokio::test]
async fn without_a_configured_token_the_internal_routes_refuse_everyone() {
    let app = common::spawn_app_with(|config| config.internal_token = None).await;
    for token in ["", "Bearer ", "Bearer anything"] {
        let response = app
            .internal_request(
                "GET",
                "/internal/settings",
                &[("Authorization", token)],
                None,
            )
            .await;
        assert_eq!(response.status, 401, "{token:?}");
    }
}

#[tokio::test]
async fn internal_paths_do_not_exist_on_the_public_port() {
    let app = common::spawn_app().await;
    let auth = format!("Bearer {TEST_INTERNAL_TOKEN}");
    let with_token = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    for (method, path) in [
        ("POST", "/internal/jobs/claim"),
        ("POST", "/internal/jobs/1/progress"),
        ("POST", "/internal/jobs/1/report"),
        ("GET", "/internal/settings"),
    ] {
        let anonymous = app.request(method, path, &[], Some("{}")).await;
        assert_eq!(anonymous.status, 404, "{method} {path}");
        let bearer = app.request(method, path, &with_token, Some("{}")).await;
        assert_eq!(bearer.status, 404, "{method} {path} with the token");
    }
    // And the public routes do not exist on the internal port.
    for path in ["/", "/healthz", "/api/me", "/api/admin/jobs"] {
        let response = app.internal_request("GET", path, &with_token, None).await;
        assert_eq!(response.status, 404, "{path}");
        assert_eq!(json_of(&response)["error"], "not_found");
    }
}

#[tokio::test]
async fn the_internal_listener_has_no_csrf_guard_and_no_browser_headers() {
    let app = common::spawn_app().await;
    // No Origin, no X-Requested-With: a service is not a browser. A write
    // is what the guard would refuse, so use one.
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 204, "{}", response.body);
    // The same write on the public port, without the headers, is refused.
    let admin = login_admin_cookie(&app).await;
    let refused = app
        .request(
            "PUT",
            "/api/admin/worker",
            &[("Cookie", &admin), ("Content-Type", "application/json")],
            Some(r#"{"enabled":true}"#),
        )
        .await;
    assert_eq!(refused.status, 403, "{}", refused.body);

    let response = worker(&app, "GET", "/internal/settings", None).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert!(response.header("content-security-policy").is_none());
    assert!(response.header("cross-origin-opener-policy").is_none());
}

#[tokio::test]
async fn the_internal_listener_is_not_per_ip_rate_limited() {
    // A low limit, so the test can fail: the public port must hit it...
    let app = common::spawn_app_with(|config| config.rate_limits.api_per_minute = 5).await;
    let mut limited = 0;
    for _ in 0..20 {
        if app.request("GET", "/api/me", &[], None).await.status == 429 {
            limited += 1;
        }
    }
    assert!(limited > 0, "the public limiter is not live in this test");
    // ...and the internal port, from the same address, must not.
    for _ in 0..20 {
        let response = worker(&app, "GET", "/internal/settings", None).await;
        assert_eq!(response.status, 200);
    }
}

// ---------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------

#[tokio::test]
async fn settings_start_off_and_follow_the_admin_toggle() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;

    let response = worker(&app, "GET", "/internal/settings", None).await;
    assert_eq!(
        json_of(&response),
        json!({"enabled": false, "mode": "dry_run"})
    );
    let response = admin_call(&app, &admin, "GET", "/api/admin/worker", None).await;
    assert_eq!(
        json_of(&response),
        json!({"enabled": false, "mode": "dry_run",
               "enabled_from_env": false, "mode_from_env": false})
    );

    let response = admin_call(
        &app,
        &admin,
        "PUT",
        "/api/admin/worker",
        Some(r#"{"enabled":true,"mode":"assisted"}"#),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(json_of(&response)["mode"], "assisted");
    let response = worker(&app, "GET", "/internal/settings", None).await;
    assert_eq!(
        json_of(&response),
        json!({"enabled": true, "mode": "assisted"})
    );

    // A partial update leaves the other switch alone.
    let response = admin_call(
        &app,
        &admin,
        "PUT",
        "/api/admin/worker",
        Some(r#"{"enabled":false}"#),
    )
    .await;
    assert_eq!(json_of(&response)["mode"], "assisted");
    assert_eq!(json_of(&response)["enabled"], false);

    for bad in [
        r#"{"mode":"turbo"}"#,
        r#"{"enabled":"yes"}"#,
        r#"{"extra":1}"#,
    ] {
        let response = admin_call(&app, &admin, "PUT", "/api/admin/worker", Some(bad)).await;
        assert_eq!(response.status, 400, "{bad}");
    }
}

#[tokio::test]
async fn an_environment_override_wins_and_the_admin_view_says_so() {
    let app = common::spawn_app_with(|config| {
        config.worker_enabled = Some(false);
        config.worker_mode = Some(mediaqueue::config::WorkerMode::Auto);
    })
    .await;
    let admin = login_admin_cookie(&app).await;
    let response = admin_call(
        &app,
        &admin,
        "PUT",
        "/api/admin/worker",
        Some(r#"{"enabled":true,"mode":"dry_run"}"#),
    )
    .await;
    assert_eq!(
        json_of(&response),
        json!({"enabled": false, "mode": "auto",
               "enabled_from_env": true, "mode_from_env": true})
    );
    let response = worker(&app, "GET", "/internal/settings", None).await;
    assert_eq!(
        json_of(&response),
        json!({"enabled": false, "mode": "auto"})
    );
}

// ---------------------------------------------------------------------
// Creating a job
// ---------------------------------------------------------------------

#[tokio::test]
async fn fetch_creates_a_job_for_a_pending_request_once() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let request = seed_request(&app, 1, "pending", Some("tt0000001"), false).await;
    let path = format!("/api/admin/requests/{request}/fetch");

    let response = admin_call(&app, &admin, "POST", &path, Some("{}")).await;
    assert_eq!(response.status, 201, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(body["job"]["state"], "queued");
    assert_eq!(body["job"]["reason"], Value::Null);
    assert_eq!(keys(&body["job"]), HashSet::from(["id", "state", "reason"]));

    let again = admin_call(&app, &admin, "POST", &path, Some("{}")).await;
    assert_error(&again, 409, "conflict");

    let refused = seed_request(&app, 2, "refused", Some("tt0000002"), false).await;
    let response = admin_call(
        &app,
        &admin,
        "POST",
        &format!("/api/admin/requests/{refused}/fetch"),
        Some("{}"),
    )
    .await;
    assert_error(&response, 409, "conflict");

    let response = admin_call(
        &app,
        &admin,
        "POST",
        "/api/admin/requests/9999/fetch",
        Some("{}"),
    )
    .await;
    assert_error(&response, 404, "not_found");
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM fetch_jobs").await, 1);
}

#[tokio::test]
async fn fetch_refuses_a_series_and_ticks_nothing() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let request = seed_request(&app, 1, "pending", Some("tt0000001"), false).await;
    sql(
        &app.pool,
        &format!("UPDATE requests SET media_type = 'tv' WHERE id = {request}"),
    )
    .await;
    let mut admins = EventStream::open(app.addr, "/api/admin/events", Some(&admin)).await;

    let response = admin_call(
        &app,
        &admin,
        "POST",
        &format!("/api/admin/requests/{request}/fetch"),
        Some("{}"),
    )
    .await;
    let body = assert_error(&response, 409, "conflict");
    assert_eq!(body["message"], "serier kan ikke hentes automatisk ennå");
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM fetch_jobs").await, 0);
    assert!(!admins.saw("event: tick", QUIET).await, "a refusal ticked");
}

#[tokio::test]
async fn a_request_without_an_imdb_id_waits_in_needs_pick() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let request = seed_request(&app, 1, "pending", None, false).await;
    let response = admin_call(
        &app,
        &admin,
        "POST",
        &format!("/api/admin/requests/{request}/fetch"),
        Some("{}"),
    )
    .await;
    assert_eq!(response.status, 201, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(body["job"]["state"], "needs_pick");
    assert_eq!(body["job"]["reason"], "no_imdb_id");
    let job = body["job"]["id"].as_i64().unwrap();

    // Retry is refused while there is no IMDb ID, and changes nothing.
    let retry = admin_call(
        &app,
        &admin,
        "POST",
        &format!("/api/admin/jobs/{job}/retry"),
        Some("{}"),
    )
    .await;
    assert_error(&retry, 409, "conflict");
    let row = job_row(&app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("no_imdb_id"))
    );
}

// ---------------------------------------------------------------------
// Claim
// ---------------------------------------------------------------------

#[tokio::test]
async fn claim_hands_out_only_the_allowed_fields_and_only_while_enabled() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let request = seed_request(&app, 7, "pending", Some("tt0000007"), true).await;
    let job = create_job(&app, &admin, request).await;
    sql(
        &app.pool,
        &format!(
            "INSERT INTO blocked_hashes (request_id, infohash, created_at)
             VALUES ({request}, '{HASH_B}', 1)"
        ),
    )
    .await;

    // Off by default: nothing is handed out and nothing is touched.
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 204);
    assert_eq!(response.body, "");
    assert_eq!(job_row(&app.pool, job).await.state, "queued");

    enable_worker(&app, &admin).await;
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(
        keys(&body),
        HashSet::from([
            "id",
            "claim_id",
            "tmdb_id",
            "imdb_id",
            "media_type",
            "title",
            "year",
            "infuse_pro",
            "chosen_infohash",
            "picked",
            "rd_torrent_id",
            "remove_rd_torrent_id",
            "blocked_hashes",
        ])
    );
    assert_eq!(body["picked"], false);
    assert_eq!(body["id"], job);
    assert_eq!(body["tmdb_id"], 7);
    assert_eq!(body["imdb_id"], "tt0000007");
    assert_eq!(body["media_type"], "movie");
    assert_eq!(body["title"], "Dune");
    assert_eq!(body["year"], 2021);
    assert_eq!(body["infuse_pro"], true);
    assert_eq!(body["blocked_hashes"], json!([HASH_B]));
    assert!(!body["claim_id"].as_str().unwrap().is_empty());
    assert!(!response.body.contains("secret note"));
    assert!(!response.body.contains("Alice"));

    // Nothing else to hand out.
    let again = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(again.status, 204);
    assert_eq!(job_row(&app.pool, job).await.state, "fetching");
}

#[tokio::test]
async fn a_stale_job_is_claimable_again_with_its_torrent_and_a_new_claim() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/progress", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "rd_torrent_id": "RD1"})),
    )
    .await;
    assert_eq!(progress.status, 200, "{}", progress.body);

    app.clock.advance(46 * MINUTE);
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(body["id"], run.job_id);
    assert_eq!(body["rd_torrent_id"], "RD1");
    assert_ne!(body["claim_id"], run.claim_id.as_str());

    // The first run's next call is stale, and names the torrent to keep.
    let stale = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "failed"})),
    )
    .await;
    let body = assert_error(&stale, 409, "stale_claim");
    assert_eq!(stored_torrent(&body), Some("RD1"));
}

#[tokio::test]
async fn a_claim_says_whether_the_hash_is_a_human_pick() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    sql(
        &app.pool,
        &format!(
            "UPDATE fetch_jobs SET picked = 1, chosen_infohash = '{HASH_A}', state = 'queued',
                    claim_id = NULL WHERE id = {}",
            run.job_id
        ),
    )
    .await;
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["picked"], true);
    assert_eq!(claim["chosen_infohash"], HASH_A);

    // A hash the worker saved itself is not a pick, even when it comes back
    // with a reclaim.
    let other = seed_request(&app, 2, "pending", Some("tt0000002"), false).await;
    let other_job = create_job(&app, &run.admin, other).await;
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["id"], other_job);
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{other_job}/progress"),
        Some(&json!({"claim_id": claim["claim_id"], "chosen_infohash": HASH_B})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    // Put the first job aside, so the reclaim below is the second one's.
    sql(
        &app.pool,
        &format!(
            "UPDATE fetch_jobs SET state = 'failed', claim_id = NULL WHERE id = {}",
            run.job_id
        ),
    )
    .await;
    app.clock.advance(46 * MINUTE);
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["id"], other_job);
    assert_eq!(claim["picked"], false);
    assert_eq!(claim["chosen_infohash"], HASH_B);
}

#[tokio::test]
async fn a_done_report_needs_a_torrent_unless_the_title_was_already_had() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let report = format!("/internal/jobs/{}/report", run.job_id);
    let progress = format!("/internal/jobs/{}/progress", run.job_id);

    // Nothing stored, nothing given: 400, and the run is left alone.
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": run.claim_id, "state": "done"})),
    )
    .await;
    assert_error(&response, 400, "bad_request");
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.state, "fetching");
    assert_eq!(row.claim_id.as_deref(), Some(run.claim_id.as_str()));
    assert_eq!(request_status(&app.pool, run.request_id).await, "pending");

    // A stale claim is still 409, not 400.
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": "old", "state": "done"})),
    )
    .await;
    assert_error(&response, 409, "stale_claim");

    // A torrent stored by a progress call is enough.
    worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": run.claim_id, "rd_torrent_id": "RD1"})),
    )
    .await;
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": run.claim_id, "state": "done"})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(request_status(&app.pool, run.request_id).await, "added");

    // already_had needs none, and sets that status.
    let other = seed_request(&app, 2, "pending", Some("tt0000002"), false).await;
    let other_job = create_job(&app, &run.admin, other).await;
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{other_job}/report"),
        Some(&json!({"claim_id": claim["claim_id"], "state": "done", "reason": "already_had"})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(request_status(&app.pool, other).await, "already_had");

    // failed and needs_pick never need one.
    let third = seed_request(&app, 3, "pending", Some("tt0000003"), false).await;
    let third_job = create_job(&app, &run.admin, third).await;
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{third_job}/report"),
        Some(&json!({"claim_id": claim["claim_id"], "state": "needs_pick", "reason": "dry_run"})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
}

#[tokio::test]
async fn a_rejected_bare_done_leaves_the_claim_usable_for_a_corrected_report() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let report = format!("/internal/jobs/{}/report", run.job_id);

    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": run.claim_id, "state": "done"})),
    )
    .await;
    assert_error(&response, 400, "bad_request");
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.state, "fetching");
    assert_eq!(row.claim_id.as_deref(), Some(run.claim_id.as_str()));
    assert_eq!(request_status(&app.pool, run.request_id).await, "pending");

    // The same claim, now with the torrent: applied.
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": run.claim_id, "state": "done", "rd_torrent_id": "RD1"})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (row.state.as_str(), row.rd_torrent_id.as_deref()),
        ("done", Some("RD1"))
    );
    assert_eq!(request_status(&app.pool, run.request_id).await, "added");
}

// ---------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------

#[tokio::test]
async fn progress_saves_ids_and_keeps_the_claim() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let path = format!("/internal/jobs/{}/progress", run.job_id);

    let response = worker(
        &app,
        "POST",
        &path,
        Some(&json!({
            "claim_id": run.claim_id,
            "rd_torrent_id": "ABC123",
            "chosen_infohash": HASH_A.to_uppercase(),
            "uncached_adds": 2,
        })),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(json_of(&response), json!({"ok": true}));
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.state, "fetching");
    assert_eq!(row.claim_id.as_deref(), Some(run.claim_id.as_str()));
    assert_eq!(row.rd_torrent_id.as_deref(), Some("ABC123"));
    assert_eq!(row.chosen_infohash.as_deref(), Some(HASH_A));
    assert_eq!(row.uncached_adds, 2);

    // A bare sign of life is fine too, and the increments add up.
    let response = worker(
        &app,
        "POST",
        &path,
        Some(&json!({"claim_id": run.claim_id, "uncached_adds": 1})),
    )
    .await;
    assert_eq!(response.status, 200);
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.uncached_adds, 3);
    assert_eq!(row.rd_torrent_id.as_deref(), Some("ABC123"));
}

#[tokio::test]
async fn progress_rejects_bad_input_without_touching_the_job() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let path = format!("/internal/jobs/{}/progress", run.job_id);
    let claim = run.claim_id.as_str();
    let long_id = "A".repeat(65);
    let bad = [
        json!({"claim_id": claim, "extra": 1}),
        json!({"claim_id": claim, "uncached_adds": 6}),
        json!({"claim_id": claim, "uncached_adds": -1}),
        json!({"claim_id": claim, "chosen_infohash": "abc"}),
        json!({"claim_id": claim, "chosen_infohash": "g".repeat(40)}),
        json!({"claim_id": claim, "rd_torrent_id": long_id}),
        json!({"claim_id": claim, "rd_torrent_id": ""}),
        json!({"claim_id": claim, "rd_torrent_id": "a/b"}),
        json!({"claim_id": claim, "removed_rd_torrent_id": ""}),
        json!({"claim_id": claim, "removed_rd_torrent_id": "a/b"}),
        json!({"claim_id": claim, "removed_rd_torrent_id": "A".repeat(65)}),
        json!({"claim_id": ""}),
        json!({"rd_torrent_id": "ABC"}),
    ];
    for body in bad {
        let response = worker(&app, "POST", &path, Some(&body)).await;
        assert_error(&response, 400, "bad_request");
    }
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.rd_torrent_id,
            row.chosen_infohash,
            row.uncached_adds
        ),
        ("fetching", None, None, 0)
    );

    let response = worker(
        &app,
        "POST",
        "/internal/jobs/9999/progress",
        Some(&json!({"claim_id": claim})),
    )
    .await;
    assert_error(&response, 404, "not_found");
    let response = worker(
        &app,
        "POST",
        "/internal/jobs/abc/progress",
        Some(&json!({"claim_id": claim})),
    )
    .await;
    assert_error(&response, 404, "not_found");
}

#[tokio::test]
async fn progress_with_a_stale_claim_is_409_and_names_the_stored_torrent() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let path = format!("/internal/jobs/{}/progress", run.job_id);

    let response = worker(
        &app,
        "POST",
        &path,
        Some(&json!({"claim_id": "not-the-claim", "rd_torrent_id": "EVIL"})),
    )
    .await;
    let body = assert_error(&response, 409, "stale_claim");
    assert_eq!(stored_torrent(&body), None);
    assert_eq!(job_row(&app.pool, run.job_id).await.rd_torrent_id, None);

    worker(
        &app,
        "POST",
        &path,
        Some(&json!({"claim_id": run.claim_id, "rd_torrent_id": "RD1"})),
    )
    .await;
    let response = worker(
        &app,
        "POST",
        &path,
        Some(&json!({"claim_id": "not-the-claim"})),
    )
    .await;
    let body = assert_error(&response, 409, "stale_claim");
    assert_eq!(stored_torrent(&body), Some("RD1"));
}

#[tokio::test]
async fn a_progress_call_at_44_minutes_keeps_the_job_from_being_reclaimed_at_50() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress = format!("/internal/jobs/{}/progress", run.job_id);

    app.clock.advance(44 * MINUTE);
    let response = worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": run.claim_id})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);

    app.clock.advance(6 * MINUTE);
    let claim = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(claim.status, 204, "reclaimed despite the progress call");
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.state, "fetching");
    assert_eq!(row.claim_id.as_deref(), Some(run.claim_id.as_str()));

    let report = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "failed", "reason": "timeout"})),
    )
    .await;
    assert_eq!(report.status, 200, "{}", report.body);
}

// ---------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_good_report_applies_once_and_the_old_claim_is_then_stale() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let path = format!("/internal/jobs/{}/report", run.job_id);
    let body = json!({
        "claim_id": run.claim_id,
        "state": "needs_pick",
        "candidates": [
            {"infohash": HASH_A.to_uppercase(), "title": "Dune 2021 1080p", "score": 9.5,
             "free_safe": "yes", "tags": ["aac", "x265"]},
            {"infohash": HASH_B, "title": "Dune 2021 2160p", "score": 8, "free_safe": "no"},
        ],
        "top_infohash": HASH_A,
        "reason": "dry_run",
        "error": "line one\nline two",
    });
    let response = worker(&app, "POST", &path, Some(&body)).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(json_of(&response), json!({"ok": true}));

    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (row.state.as_str(), row.claim_id, row.reason.as_deref()),
        ("needs_pick", None, Some("dry_run"))
    );
    let stored: (String, String, String) =
        sqlx::query_as("SELECT candidates, top_infohash, last_error FROM fetch_jobs WHERE id = ?")
            .bind(run.job_id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    let candidates: Value = serde_json::from_str(&stored.0).unwrap();
    assert_eq!(candidates[0]["infohash"], HASH_A);
    assert_eq!(candidates[0]["tags"], json!(["aac", "x265"]));
    assert_eq!(candidates.as_array().unwrap().len(), 2);
    assert_eq!(stored.1, HASH_A);
    assert_eq!(stored.2, "line oneline two");
    // The request is untouched by anything but a done report.
    assert_eq!(request_status(&app.pool, run.request_id).await, "pending");

    // A replay is stale.
    let replay = worker(&app, "POST", &path, Some(&body)).await;
    assert_error(&replay, 409, "stale_claim");
    // So is a wrong claim id on a job that was never claimed that way.
    let other = seed_request(&app, 2, "pending", Some("tt0000002"), false).await;
    let other_job = create_job(&app, &run.admin, other).await;
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["id"], other_job);
    let wrong = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{other_job}/report"),
        Some(&json!({"claim_id": run.claim_id, "state": "failed"})),
    )
    .await;
    assert_error(&wrong, 409, "stale_claim");
    assert_eq!(job_row(&app.pool, other_job).await.state, "fetching");
}

#[tokio::test]
async fn a_report_with_bad_input_is_400_and_leaves_the_run_alone() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let path = format!("/internal/jobs/{}/report", run.job_id);
    let claim = run.claim_id.as_str();
    let candidate = |n: usize| json!({"title": format!("c{n}"), "score": n});
    let bad = [
        json!({"claim_id": claim, "state": "needs_pick", "top_infohash": "abc"}),
        json!({"claim_id": claim, "state": "needs_pick", "chosen_infohash": "z".repeat(40)}),
        json!({"claim_id": claim, "state": "needs_pick",
               "candidates": (0..21).map(candidate).collect::<Vec<_>>()}),
        json!({"claim_id": claim, "state": "needs_pick", "candidates": [{"infohash": "short"}]}),
        json!({"claim_id": claim, "state": "needs_pick", "candidates": [{"deep": {"a": 1}}]}),
        json!({"claim_id": claim, "state": "needs_pick", "candidates": [{"t": "x".repeat(501)}]}),
        // Twenty entries that pass one by one but together exceed 64 KiB.
        json!({"claim_id": claim, "state": "needs_pick",
               "candidates": (0..20).map(|_| {
                   (0..10).map(|k| (format!("k{k}"), json!("x".repeat(500))))
                       .collect::<serde_json::Map<_, _>>()
               }).collect::<Vec<_>>()}),
        json!({"claim_id": claim, "state": "failed", "extra": true}),
        json!({"claim_id": claim, "state": "queued"}),
        json!({"claim_id": claim, "state": "fetching"}),
        json!({"claim_id": claim, "state": "bogus"}),
        json!({"claim_id": claim}),
        json!({"state": "failed"}),
        json!({"claim_id": claim, "state": "failed", "reason": "cancelled"}),
        json!({"claim_id": claim, "state": "failed", "error": "x".repeat(501)}),
        json!({"claim_id": claim, "state": "failed", "rd_torrent_id": "A".repeat(65)}),
    ];
    for body in bad {
        let response = worker(&app, "POST", &path, Some(&body)).await;
        assert_error(&response, 400, "bad_request");
    }
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.state, "fetching");
    assert_eq!(row.claim_id.as_deref(), Some(claim));

    // The cut-off for the whole body is the listener's 256 KiB limit.
    let huge = json!({"claim_id": claim, "state": "failed", "error": "x".repeat(300 * 1024)});
    let response = worker(&app, "POST", &path, Some(&huge)).await;
    assert_eq!(response.status, 413);
    assert_eq!(job_row(&app.pool, run.job_id).await.state, "fetching");

    // Every accepted reason works.
    let ok = worker(
        &app,
        "POST",
        &path,
        Some(
            &json!({"claim_id": claim, "state": "failed", "reason": "rd_error",
                      "error": "boom"}),
        ),
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
    assert_eq!(job_row(&app.pool, run.job_id).await.state, "failed");
}

#[tokio::test]
async fn a_report_for_an_unknown_job_is_404() {
    let app = common::spawn_app().await;
    let response = worker(
        &app,
        "POST",
        "/internal/jobs/9999/report",
        Some(&json!({"claim_id": "x", "state": "failed"})),
    )
    .await;
    assert_error(&response, 404, "not_found");
}

#[tokio::test]
async fn done_marks_the_request_added_as_the_worker() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "done",
                      "chosen_infohash": HASH_A, "rd_torrent_id": "RD1"})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(request_status(&app.pool, run.request_id).await, "added");
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (row.state.as_str(), row.rd_torrent_id.as_deref()),
        ("done", Some("RD1"))
    );
    let event: (String, String, String) = sqlx::query_as(
        "SELECT actor, from_status, to_status FROM request_events
         WHERE request_id = ? AND kind = 'status'",
    )
    .bind(run.request_id)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(event, ("worker".into(), "pending".into(), "added".into()));
}

#[tokio::test]
async fn done_leaves_a_request_that_is_no_longer_pending_alone() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    // Changed behind the portal's back, so the job was not closed.
    sql(
        &app.pool,
        &format!(
            "UPDATE requests SET status = 'refused' WHERE id = {}",
            run.request_id
        ),
    )
    .await;
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "done", "rd_torrent_id": "RD1"})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(request_status(&app.pool, run.request_id).await, "refused");
    assert_eq!(
        count(
            &app.pool,
            "SELECT COUNT(*) FROM request_events WHERE actor = 'worker'"
        )
        .await,
        0
    );
}

// ---------------------------------------------------------------------
// The picked lock
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_picked_job_keeps_its_hash_against_progress_and_report() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    sql(
        &app.pool,
        &format!(
            "UPDATE fetch_jobs SET picked = 1, chosen_infohash = '{HASH_A}' WHERE id = {}",
            run.job_id
        ),
    )
    .await;
    let progress = format!("/internal/jobs/{}/progress", run.job_id);
    let report = format!("/internal/jobs/{}/report", run.job_id);
    let claim = run.claim_id.as_str();

    // A different hash is 400 picked_locked, on both calls.
    let response = worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": claim, "chosen_infohash": HASH_B})),
    )
    .await;
    assert_error(&response, 400, "picked_locked");
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": claim, "state": "needs_pick", "chosen_infohash": HASH_B})),
    )
    .await;
    assert_error(&response, 400, "picked_locked");
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.claim_id.as_deref(),
            row.chosen_infohash.as_deref()
        ),
        ("fetching", Some(claim), Some(HASH_A))
    );

    // The same hash is fine, and so is leaving it out.
    let response = worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": claim, "chosen_infohash": HASH_A})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);

    // A stale claim is reported before the lock.
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": "old", "state": "needs_pick", "chosen_infohash": HASH_B})),
    )
    .await;
    assert_error(&response, 409, "stale_claim");

    // picked_failed is accepted and clears the torrent.
    worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": claim, "rd_torrent_id": "RD1"})),
    )
    .await;
    let response = worker(
        &app,
        "POST",
        &report,
        Some(
            &json!({"claim_id": claim, "state": "needs_pick", "reason": "picked_failed",
                      "chosen_infohash": HASH_A}),
        ),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (row.state.as_str(), row.rd_torrent_id, row.reason.as_deref()),
        ("needs_pick", None, Some("picked_failed"))
    );
}

#[tokio::test]
async fn an_unpicked_job_may_change_its_hash() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress = format!("/internal/jobs/{}/progress", run.job_id);
    for hash in [HASH_A, HASH_B] {
        let response = worker(
            &app,
            "POST",
            &progress,
            Some(&json!({"claim_id": run.claim_id, "chosen_infohash": hash})),
        )
        .await;
        assert_eq!(response.status, 200, "{}", response.body);
    }
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "needs_pick",
                      "chosen_infohash": HASH_A})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        job_row(&app.pool, run.job_id)
            .await
            .chosen_infohash
            .as_deref(),
        Some(HASH_A)
    );
}

/// The hashes blocked for a request, sorted.
async fn blocked_for(pool: &SqlitePool, request_id: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT infohash FROM blocked_hashes WHERE request_id = ? ORDER BY infohash")
        .bind(request_id)
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn blocked_hashes_on_progress_and_on_report_are_stored_and_listed_on_the_next_claim() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress = format!("/internal/jobs/{}/progress", run.job_id);
    let report = format!("/internal/jobs/{}/report", run.job_id);

    let response = worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": run.claim_id, "blocked": [HASH_A.to_uppercase()]})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(blocked_for(&app.pool, run.request_id).await, [HASH_A]);

    // Twice is once, and the report may add more.
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": run.claim_id, "state": "needs_pick",
                      "blocked": [HASH_A, HASH_B]})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        blocked_for(&app.pool, run.request_id).await,
        [HASH_A, HASH_B]
    );

    let retry = admin_call(
        &app,
        &run.admin,
        "POST",
        &format!("/api/admin/jobs/{}/retry", run.job_id),
        Some("{}"),
    )
    .await;
    assert_eq!(retry.status, 200, "{}", retry.body);
    let claim = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(claim.status, 200, "{}", claim.body);
    assert_eq!(json_of(&claim)["blocked_hashes"], json!([HASH_A, HASH_B]));
}

#[tokio::test]
async fn a_stale_claim_blocks_nothing() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress = format!("/internal/jobs/{}/progress", run.job_id);
    let report = format!("/internal/jobs/{}/report", run.job_id);
    let old = run.claim_id.as_str();

    let response = worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": "not-the-claim", "blocked": [HASH_A]})),
    )
    .await;
    assert_error(&response, 409, "stale_claim");
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": "not-the-claim", "state": "failed", "blocked": [HASH_A]})),
    )
    .await;
    assert_error(&response, 409, "stale_claim");

    // A claim that was valid, replayed after its report.
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": old, "state": "needs_pick"})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let response = worker(
        &app,
        "POST",
        &report,
        Some(&json!({"claim_id": old, "state": "needs_pick", "blocked": [HASH_B]})),
    )
    .await;
    assert_error(&response, 409, "stale_claim");
    assert!(blocked_for(&app.pool, run.request_id).await.is_empty());
}

#[tokio::test]
async fn too_many_or_malformed_blocked_hashes_are_400_and_block_nothing() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress = format!("/internal/jobs/{}/progress", run.job_id);
    let report = format!("/internal/jobs/{}/report", run.job_id);
    let claim = run.claim_id.as_str();
    let six: Vec<String> = ["1", "2", "3", "4", "5", "6"]
        .iter()
        .map(|digit| digit.repeat(40))
        .collect();
    let five = &six[..5];
    for blocked in [json!(six), json!([HASH_A, "abc"]), json!(["g".repeat(40)])] {
        let response = worker(
            &app,
            "POST",
            &progress,
            Some(&json!({"claim_id": claim, "blocked": blocked})),
        )
        .await;
        assert_error(&response, 400, "bad_request");
        let response = worker(
            &app,
            "POST",
            &report,
            Some(&json!({"claim_id": claim, "state": "failed", "blocked": blocked})),
        )
        .await;
        assert_error(&response, 400, "bad_request");
    }
    assert!(blocked_for(&app.pool, run.request_id).await.is_empty());
    assert_eq!(job_row(&app.pool, run.job_id).await.state, "fetching");

    // Five is the limit.
    let response = worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": claim, "blocked": five})),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(blocked_for(&app.pool, run.request_id).await.len(), 5);
}

#[tokio::test]
async fn a_picked_job_blocks_nothing_through_a_refused_hash_change() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    sql(
        &app.pool,
        &format!(
            "UPDATE fetch_jobs SET picked = 1, chosen_infohash = '{HASH_A}' WHERE id = {}",
            run.job_id
        ),
    )
    .await;
    let claim = run.claim_id.as_str();
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/progress", run.job_id),
        Some(&json!({"claim_id": claim, "chosen_infohash": HASH_B, "blocked": [HASH_A]})),
    )
    .await;
    assert_error(&response, 400, "picked_locked");
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": claim, "state": "needs_pick",
                      "chosen_infohash": HASH_B, "blocked": [HASH_A]})),
    )
    .await;
    assert_error(&response, 400, "picked_locked");
    assert!(blocked_for(&app.pool, run.request_id).await.is_empty());
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.chosen_infohash.as_deref(), Some(HASH_A));
    assert_eq!(row.state, "fetching");
}

// ---------------------------------------------------------------------
// Admin decisions under a running job
// ---------------------------------------------------------------------

#[tokio::test]
async fn refusing_or_accepting_the_request_makes_the_workers_calls_stale() {
    for status in ["refused", "added"] {
        let app = common::spawn_app().await;
        let run = running(&app).await;
        let report = format!("/internal/jobs/{}/report", run.job_id);
        let progress = format!("/internal/jobs/{}/progress", run.job_id);
        worker(
            &app,
            "POST",
            &progress,
            Some(&json!({"claim_id": run.claim_id, "rd_torrent_id": "RD1"})),
        )
        .await;

        let patch = admin_call(
            &app,
            &run.admin,
            "PATCH",
            &format!("/api/admin/requests/{}", run.request_id),
            Some(&format!(r#"{{"status":"{status}"}}"#)),
        )
        .await;
        assert_eq!(patch.status, 200, "{}", patch.body);

        let response = worker(
            &app,
            "POST",
            &report,
            Some(&json!({"claim_id": run.claim_id, "state": "done"})),
        )
        .await;
        let body = assert_error(&response, 409, "stale_claim");
        assert_eq!(stored_torrent(&body), Some("RD1"), "{status}");
        let response = worker(
            &app,
            "POST",
            &progress,
            Some(&json!({"claim_id": run.claim_id})),
        )
        .await;
        assert_error(&response, 409, "stale_claim");

        let row = job_row(&app.pool, run.job_id).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref()),
            ("done", Some("request_changed")),
            "{status}"
        );
        // The admin's decision stands.
        assert_eq!(request_status(&app.pool, run.request_id).await, status);

        // Reopened, the job can run again; a second job is still refused.
        let reopen = admin_call(
            &app,
            &run.admin,
            "PATCH",
            &format!("/api/admin/requests/{}", run.request_id),
            Some(r#"{"status":"pending"}"#),
        )
        .await;
        assert_eq!(reopen.status, 200);
        let retry = admin_call(
            &app,
            &run.admin,
            "POST",
            &format!("/api/admin/jobs/{}/retry", run.job_id),
            Some("{}"),
        )
        .await;
        assert_eq!(retry.status, 200, "{}", retry.body);
        let again = admin_call(
            &app,
            &run.admin,
            "POST",
            &format!("/api/admin/requests/{}/fetch", run.request_id),
            Some("{}"),
        )
        .await;
        assert_error(&again, 409, "conflict");
    }
}

#[tokio::test]
async fn retry_and_cancel_make_the_running_workers_calls_stale() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let report = format!("/internal/jobs/{}/report", run.job_id);
    let progress = format!("/internal/jobs/{}/progress", run.job_id);
    let retry_path = format!("/api/admin/jobs/{}/retry", run.job_id);
    let cancel_path = format!("/api/admin/jobs/{}/cancel", run.job_id);
    worker(
        &app,
        "POST",
        &progress,
        Some(&json!({"claim_id": run.claim_id, "rd_torrent_id": "RD1"})),
    )
    .await;

    // Retry: queued again, torrent kept for the next run.
    let response = admin_call(&app, &run.admin, "POST", &retry_path, Some("{}")).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        json_of(&response),
        json!({"id": run.job_id, "state": "queued"})
    );
    for (path, body) in [
        (&report, json!({"claim_id": run.claim_id, "state": "done"})),
        (&progress, json!({"claim_id": run.claim_id})),
    ] {
        let response = worker(&app, "POST", path, Some(&body)).await;
        let error = assert_error(&response, 409, "stale_claim");
        assert_eq!(stored_torrent(&error), Some("RD1"));
    }
    assert_eq!(request_status(&app.pool, run.request_id).await, "pending");

    // Claimed again, then cancelled: terminal.
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["rd_torrent_id"], "RD1");
    let second = claim["claim_id"].as_str().unwrap().to_string();
    let response = admin_call(&app, &run.admin, "POST", &cancel_path, Some("{}")).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        json_of(&response),
        json!({"id": run.job_id, "state": "failed"})
    );
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref(), row.claim_id),
        ("failed", Some("cancelled"), None)
    );
    for (path, body) in [
        (&report, json!({"claim_id": second, "state": "done"})),
        (&progress, json!({"claim_id": second})),
    ] {
        let response = worker(&app, "POST", path, Some(&body)).await;
        // The torrent moved to the removal column, so the stale answer names
        // none for the worker to keep.
        let error = assert_error(&response, 409, "stale_claim");
        assert_eq!(stored_torrent(&error), None);
    }
    assert_eq!(request_status(&app.pool, run.request_id).await, "pending");
    assert_eq!(
        removal_column(&app.pool, run.job_id).await.as_deref(),
        Some("RD1")
    );
    assert_eq!(job_row(&app.pool, run.job_id).await.rd_torrent_id, None);
}

#[tokio::test]
async fn cancel_refuses_a_job_that_is_not_fetching_but_holds_a_torrent() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/progress", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "rd_torrent_id": "RD1"})),
    )
    .await;
    // Retried: queued again with the torrent still named.
    let retry = format!("/api/admin/jobs/{}/retry", run.job_id);
    let response = admin_call(&app, &run.admin, "POST", &retry, Some("{}")).await;
    assert_eq!(response.status, 200, "{}", response.body);

    let cancel = format!("/api/admin/jobs/{}/cancel", run.job_id);
    let response = admin_call(&app, &run.admin, "POST", &cancel, Some("{}")).await;
    let error = assert_error(&response, 409, "conflict");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("Real-Debrid")),
        "{error}"
    );
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (row.state.as_str(), row.rd_torrent_id.as_deref()),
        ("queued", Some("RD1"))
    );
    assert_eq!(removal_column(&app.pool, run.job_id).await, None);
}

#[tokio::test]
async fn retry_and_cancel_check_the_state_and_the_request() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let request = seed_request(&app, 1, "pending", Some("tt0000001"), false).await;
    let job = create_job(&app, &admin, request).await;
    let retry = format!("/api/admin/jobs/{job}/retry");
    let cancel = format!("/api/admin/jobs/{job}/cancel");

    // A queued job has nothing to retry.
    assert_error(
        &admin_call(&app, &admin, "POST", &retry, Some("{}")).await,
        409,
        "conflict",
    );
    // Unknown ids.
    for path in ["/api/admin/jobs/9999/retry", "/api/admin/jobs/9999/cancel"] {
        assert_error(
            &admin_call(&app, &admin, "POST", path, Some("{}")).await,
            404,
            "not_found",
        );
    }

    // Retry from done works while the request is pending, and not after.
    sql(
        &app.pool,
        "UPDATE fetch_jobs SET state = 'done', reason = 'dry_run'",
    )
    .await;
    assert_error(
        &admin_call(&app, &admin, "POST", &cancel, Some("{}")).await,
        409,
        "conflict",
    );
    let response = admin_call(&app, &admin, "POST", &retry, Some("{}")).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(job_row(&app.pool, job).await.state, "queued");

    sql(&app.pool, "UPDATE fetch_jobs SET state = 'done'").await;
    sql(&app.pool, "UPDATE requests SET status = 'added'").await;
    assert_error(
        &admin_call(&app, &admin, "POST", &retry, Some("{}")).await,
        409,
        "conflict",
    );
    // And a failed job whose request was settled by hand is not retried
    // either.
    sql(&app.pool, "UPDATE fetch_jobs SET state = 'failed'").await;
    assert_error(
        &admin_call(&app, &admin, "POST", &retry, Some("{}")).await,
        409,
        "conflict",
    );
    assert_eq!(job_row(&app.pool, job).await.state, "failed");
}

/// A pending request whose job waits in `needs_pick` with a shortlist of
/// `HASH_A` and `HASH_B`; returns the request and job ids.
async fn needs_pick_job(app: &TestApp, admin: &str, tmdb_id: i64) -> (i64, i64) {
    let request = seed_request(app, tmdb_id, "pending", Some("tt0000001"), false).await;
    let job = create_job(app, admin, request).await;
    sql(
        &app.pool,
        &format!(
            "UPDATE fetch_jobs SET state = 'needs_pick', reason = 'dry_run',
                    candidates = '[{{\"infohash\":\"{HASH_A}\"}},{{\"infohash\":\"{HASH_B}\"}}]'
             WHERE id = {job}"
        ),
    )
    .await;
    (request, job)
}

fn pick_body(hash: &str) -> String {
    json!({ "infohash": hash }).to_string()
}

#[tokio::test]
async fn a_pick_queues_the_job_in_a_live_mode_and_closes_it_in_dry_run() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    enable_worker(&app, &admin).await;
    let (request, job) = needs_pick_job(&app, &admin, 1).await;
    let (dry_request, dry_job) = needs_pick_job(&app, &admin, 2).await;

    // The mode is read at pick time: dry-run is the default.
    let path = format!("/api/admin/jobs/{dry_job}/pick");
    let response = admin_call(&app, &admin, "POST", &path, Some(&pick_body(HASH_B))).await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(
        (body["id"].as_i64(), body["state"].as_str(), &body["picked"]),
        (Some(dry_job), Some("done"), &json!(true))
    );
    let row = job_row(&app.pool, dry_job).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.reason.as_deref(),
            row.chosen_infohash.as_deref()
        ),
        ("done", Some("dry_run"), Some(HASH_B))
    );
    assert_eq!(request_status(&app.pool, dry_request).await, "pending");

    let response = admin_call(
        &app,
        &admin,
        "PUT",
        "/api/admin/worker",
        Some(r#"{"mode":"assisted"}"#),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let path = format!("/api/admin/jobs/{job}/pick");
    let response = admin_call(&app, &admin, "POST", &path, Some(&pick_body(HASH_A))).await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(
        (body["id"].as_i64(), body["state"].as_str(), &body["picked"]),
        (Some(job), Some("queued"), &json!(true))
    );
    assert_eq!(request_status(&app.pool, request).await, "pending");

    // The worker's next claim carries the pick, flagged as human.
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["id"], job);
    assert_eq!(claim["chosen_infohash"], HASH_A);
    assert_eq!(claim["picked"], true);
}

#[tokio::test]
async fn a_pick_checks_its_input_the_job_and_the_list() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let (request, job) = needs_pick_job(&app, &admin, 1).await;
    let path = format!("/api/admin/jobs/{job}/pick");
    let pick = |body: String| {
        let (app, admin, path) = (&app, &admin, &path);
        async move { admin_call(app, admin, "POST", path, Some(&body)).await }
    };

    for bad in [
        r#"{}"#.to_string(),
        r#"{"infohash":"abc"}"#.to_string(),
        r#"{"infohash":7}"#.to_string(),
        format!(r#"{{"infohash":"{HASH_A}","extra":1}}"#),
        "not json".to_string(),
    ] {
        assert_error(&pick(bad.clone()).await, 400, "bad_request");
        assert_eq!(job_row(&app.pool, job).await.state, "needs_pick", "{bad}");
    }
    assert_error(
        &admin_call(
            &app,
            &admin,
            "POST",
            "/api/admin/jobs/9999/pick",
            Some(&pick_body(HASH_A)),
        )
        .await,
        404,
        "not_found",
    );

    // Not in the list; blocked for this request.
    assert_error(&pick(pick_body(&"c".repeat(40))).await, 409, "conflict");
    sql(
        &app.pool,
        &format!("INSERT INTO blocked_hashes VALUES ({request}, '{HASH_A}', 1)"),
    )
    .await;
    assert_error(&pick(pick_body(HASH_A)).await, 409, "conflict");
    // The upper-case spelling of a listed hash is the same hash.
    assert_error(
        &pick(pick_body(&HASH_A.to_uppercase())).await,
        409,
        "conflict",
    );
    let row = job_row(&app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.chosen_infohash),
        ("needs_pick", None)
    );

    // Only a job that waits for a pick can be picked.
    sql(&app.pool, "UPDATE fetch_jobs SET state = 'failed'").await;
    assert_error(&pick(pick_body(HASH_B)).await, 409, "conflict");
    assert_eq!(job_row(&app.pool, job).await.state, "failed");
}

#[tokio::test]
async fn a_pick_ticks_the_admin_only_when_it_applies() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let (_, job) = needs_pick_job(&app, &admin, 1).await;
    let mut admins = EventStream::open(app.addr, "/api/admin/events", Some(&admin)).await;
    let mut members = {
        let member = common::login_new_member(&app, "Bob").await;
        EventStream::open(app.addr, "/api/events", Some(&member)).await
    };
    let path = format!("/api/admin/jobs/{job}/pick");

    let response = admin_call(
        &app,
        &admin,
        "POST",
        &path,
        Some(&pick_body(&"c".repeat(40))),
    )
    .await;
    assert_eq!(response.status, 409);
    assert!(!admins.saw("event: tick", QUIET).await, "a refusal ticked");

    let response = admin_call(&app, &admin, "POST", &path, Some(&pick_body(HASH_A))).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(admins.saw("event: tick", SOON).await, "no admin tick");
    assert!(!members.saw("event: tick", QUIET).await);
}

#[tokio::test]
async fn not_playing_makes_the_running_workers_next_report_stale() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/progress", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "chosen_infohash": HASH_A})),
    )
    .await;
    assert_eq!(progress.status, 200, "{}", progress.body);
    assert_eq!(job_row(&app.pool, run.job_id).await.state, "fetching");

    let response = admin_call(
        &app,
        &run.admin,
        "POST",
        &format!("/api/admin/jobs/{}/not-playing", run.job_id),
        Some("{}"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(
        (
            body["id"].as_i64(),
            body["state"].as_str(),
            &body["reopened"]
        ),
        (Some(run.job_id), Some("queued"), &json!(false))
    );
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(
        (row.state.as_str(), row.claim_id, row.chosen_infohash),
        ("queued", None, None)
    );

    let report = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "needs_pick", "reason": "dry_run"})),
    )
    .await;
    assert_error(&report, 409, "stale_claim");

    // The next claim is a new run that excludes the hash.
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["id"], run.job_id);
    assert_eq!(claim["blocked_hashes"], json!([HASH_A]));
    assert_eq!(claim["chosen_infohash"], Value::Null);
}

async fn removal_column(pool: &SqlitePool, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT remove_rd_torrent_id FROM fetch_jobs WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn not_playing_hands_the_torrent_to_the_next_claim_and_progress_clears_it() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let progress_path = format!("/internal/jobs/{}/progress", run.job_id);
    let progress = worker(
        &app,
        "POST",
        &progress_path,
        Some(&json!({
            "claim_id": run.claim_id,
            "chosen_infohash": HASH_A,
            "rd_torrent_id": "RD1",
        })),
    )
    .await;
    assert_eq!(progress.status, 200, "{}", progress.body);

    let response = admin_call(
        &app,
        &run.admin,
        "POST",
        &format!("/api/admin/jobs/{}/not-playing", run.job_id),
        Some("{}"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    let row = job_row(&app.pool, run.job_id).await;
    assert_eq!(row.rd_torrent_id, None);
    assert_eq!(
        removal_column(&app.pool, run.job_id).await.as_deref(),
        Some("RD1")
    );

    // The old run is stale and cannot clear anything.
    let stale = worker(
        &app,
        "POST",
        &progress_path,
        Some(&json!({"claim_id": run.claim_id, "removed_rd_torrent_id": "RD1"})),
    )
    .await;
    assert_error(&stale, 409, "stale_claim");
    assert_eq!(
        removal_column(&app.pool, run.job_id).await.as_deref(),
        Some("RD1")
    );

    // The next claim carries the removal and no torrent to resume.
    let claim = json_of(&worker(&app, "POST", "/internal/jobs/claim", None).await);
    assert_eq!(claim["id"], run.job_id);
    assert_eq!(claim["remove_rd_torrent_id"], "RD1");
    assert_eq!(claim["rd_torrent_id"], Value::Null);
    let claim_id = claim["claim_id"].as_str().unwrap();

    // Naming another torrent clears nothing; naming this one does.
    for (removed, left) in [("OTHER", Some("RD1")), ("RD1", None)] {
        let response = worker(
            &app,
            "POST",
            &progress_path,
            Some(&json!({"claim_id": claim_id, "removed_rd_torrent_id": removed})),
        )
        .await;
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(
            removal_column(&app.pool, run.job_id).await.as_deref(),
            left,
            "{removed}"
        );
    }
}

#[tokio::test]
async fn not_playing_checks_the_job_and_the_request() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let request = seed_request(&app, 1, "pending", Some("tt0000001"), false).await;
    let job = create_job(&app, &admin, request).await;
    let path = format!("/api/admin/jobs/{job}/not-playing");

    assert_error(
        &admin_call(
            &app,
            &admin,
            "POST",
            "/api/admin/jobs/9999/not-playing",
            Some("{}"),
        )
        .await,
        404,
        "not_found",
    );
    // No chosen hash.
    assert_error(
        &admin_call(&app, &admin, "POST", &path, Some("{}")).await,
        409,
        "conflict",
    );

    sql(
        &app.pool,
        &format!("UPDATE fetch_jobs SET chosen_infohash = '{HASH_A}', picked = 1"),
    )
    .await;
    sql(&app.pool, "UPDATE requests SET status = 'refused'").await;
    assert_error(
        &admin_call(&app, &admin, "POST", &path, Some("{}")).await,
        409,
        "conflict",
    );

    assert_eq!(
        count(&app.pool, "SELECT COUNT(*) FROM blocked_hashes").await,
        0
    );
    let row = job_row(&app.pool, job).await;
    assert_eq!(row.chosen_infohash.as_deref(), Some(HASH_A));
}

#[tokio::test]
async fn not_playing_ticks_the_members_only_when_it_reopens_the_request() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let first = seed_request(&app, 1, "pending", Some("tt0000001"), false).await;
    let second = seed_request(&app, 2, "pending", Some("tt0000002"), false).await;
    let pending_job = create_job(&app, &admin, first).await;
    let added_job = create_job(&app, &admin, second).await;
    sql(
        &app.pool,
        &format!("UPDATE fetch_jobs SET state = 'done', chosen_infohash = '{HASH_A}', picked = 1"),
    )
    .await;
    sql(
        &app.pool,
        &format!("UPDATE requests SET status = 'added' WHERE id = {second}"),
    )
    .await;
    let mut admins = EventStream::open(app.addr, "/api/admin/events", Some(&admin)).await;
    let mut members = {
        let member = common::login_new_member(&app, "Bob").await;
        EventStream::open(app.addr, "/api/events", Some(&member)).await
    };

    let response = admin_call(
        &app,
        &admin,
        "POST",
        &format!("/api/admin/jobs/{pending_job}/not-playing"),
        Some("{}"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(admins.saw("event: tick", SOON).await, "no admin tick");
    assert!(!members.saw("event: tick", QUIET).await);

    let response = admin_call(
        &app,
        &admin,
        "POST",
        &format!("/api/admin/jobs/{added_job}/not-playing"),
        Some("{}"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(json_of(&response)["reopened"], true);
    assert_eq!(request_status(&app.pool, second).await, "pending");
    assert!(members.saw("event: tick", SOON).await, "no members tick");
}

#[tokio::test]
async fn deleting_the_request_removes_its_job() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let response = admin_call(
        &app,
        &run.admin,
        "DELETE",
        &format!("/api/admin/requests/{}", run.request_id),
        None,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM fetch_jobs").await, 0);
    // The worker finds out as a 404.
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "done"})),
    )
    .await;
    assert_error(&response, 404, "not_found");
}

/// `jobs_needs_pick` from the admin counts.
async fn waiting(app: &TestApp, admin: &str) -> Value {
    let response = app
        .request("GET", "/api/admin/counts", &[("Cookie", admin)], None)
        .await;
    json_of(&response)["jobs_needs_pick"].clone()
}

#[tokio::test]
async fn the_counts_report_jobs_waiting_for_a_pick() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    assert_eq!(waiting(&app, &admin).await, json!(0));

    // Without an IMDb ID the job waits for the admin from the start.
    let request = seed_request(&app, 1, "pending", None, false).await;
    let job = create_job(&app, &admin, request).await;
    assert_eq!(waiting(&app, &admin).await, json!(1));

    let response = admin_call(
        &app,
        &admin,
        "POST",
        &format!("/api/admin/jobs/{job}/cancel"),
        Some("{}"),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(waiting(&app, &admin).await, json!(0));
}

// ---------------------------------------------------------------------
// Ticks
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_report_ticks_the_admin_and_progress_does_not() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let mut admins = EventStream::open(app.addr, "/api/admin/events", Some(&run.admin)).await;
    let mut members = {
        let member = common::login_new_member(&app, "Bob").await;
        EventStream::open(app.addr, "/api/events", Some(&member)).await
    };

    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/progress", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "rd_torrent_id": "RD1"})),
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(
        !admins.saw("event: tick", QUIET).await,
        "progress must not tick"
    );

    // A report that leaves the request alone ticks the admin only.
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "needs_pick", "reason": "dry_run"})),
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(admins.saw("event: tick", SOON).await, "no admin tick");
    assert!(!members.saw("event: tick", QUIET).await);
}

#[tokio::test]
async fn a_done_report_that_adds_the_request_ticks_the_members_too() {
    let app = common::spawn_app().await;
    let run = running(&app).await;
    let member = common::login_new_member(&app, "Bob").await;
    let mut members = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    let response = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{}/report", run.job_id),
        Some(&json!({"claim_id": run.claim_id, "state": "done", "rd_torrent_id": "RD1"})),
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(members.saw("event: tick", SOON).await, "no members tick");
}

#[tokio::test]
async fn claims_tick_the_admin_only_when_something_changed() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    enable_worker(&app, &admin).await;
    let mut admins = EventStream::open(app.addr, "/api/admin/events", Some(&admin)).await;

    // Idle polls change nothing and tick nothing.
    for _ in 0..3 {
        let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
        assert_eq!(response.status, 204);
    }
    assert!(
        !admins.saw("event: tick", QUIET).await,
        "an idle poll ticked"
    );

    // A job handed out ticks once the job exists (creating it ticks too).
    let request = seed_request(&app, 1, "pending", Some("tt0000001"), false).await;
    let job = create_job(&app, &admin, request).await;
    assert!(admins.saw("event: tick", SOON).await, "no tick for the job");
    assert!(!admins.saw("event: tick", QUIET).await);
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(
        admins.saw("event: tick", SOON).await,
        "no tick for the claim"
    );
    assert!(!admins.saw("event: tick", QUIET).await);

    // The tidy-up alone ticks too: an expired job out of attempts fails,
    // and nothing is handed out.
    sql(&app.pool, "UPDATE fetch_jobs SET max_attempts = 1").await;
    app.clock.advance(46 * MINUTE);
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 204);
    assert!(
        admins.saw("event: tick", SOON).await,
        "no tick for max_attempts"
    );
    let row = job_row(&app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("failed", Some("max_attempts"))
    );
    // The next poll is idle again.
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 204);
    assert!(!admins.saw("event: tick", QUIET).await);

    // A queued job whose request changed behind the portal's back is closed
    // by the tidy-up, with a tick and no claim.
    let other = seed_request(&app, 2, "pending", Some("tt0000002"), false).await;
    let other_job = create_job(&app, &admin, other).await;
    assert!(admins.saw("event: tick", SOON).await);
    sql(
        &app.pool,
        &format!("UPDATE requests SET status = 'refused' WHERE id = {other}"),
    )
    .await;
    let response = worker(&app, "POST", "/internal/jobs/claim", None).await;
    assert_eq!(response.status, 204);
    assert!(
        admins.saw("event: tick", SOON).await,
        "no tick for request_changed"
    );
    let row = job_row(&app.pool, other_job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("done", Some("request_changed"))
    );
}

// ---------------------------------------------------------------------
// The jobs list
// ---------------------------------------------------------------------

#[tokio::test]
async fn the_jobs_list_pages_filters_and_parses_candidates() {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let mut jobs = Vec::new();
    for n in 1..=3 {
        let imdb = (n != 3).then(|| format!("tt000000{n}"));
        let request = seed_request(&app, n, "pending", imdb.as_deref(), n == 2).await;
        jobs.push(create_job(&app, &admin, request).await);
        app.clock.advance(Duration::from_secs(1));
    }
    sql(
        &app.pool,
        &format!(
            r#"UPDATE fetch_jobs SET candidates = '[{{"title":"A","score":1}}]', top_infohash = '{HASH_A}',
                   reason = 'dry_run', state = 'needs_pick' WHERE id = {}"#,
            jobs[0]
        ),
    )
    .await;
    sql(
        &app.pool,
        &format!(
            "UPDATE fetch_jobs SET candidates = 'not json {{' WHERE id = {}",
            jobs[1]
        ),
    )
    .await;
    sql(
        &app.pool,
        &format!(
            "INSERT INTO blocked_hashes (request_id, infohash, created_at)
             SELECT request_id, '{hash}', 1 FROM fetch_jobs WHERE id = {job}",
            hash = HASH_A,
            job = jobs[0]
        ),
    )
    .await;
    sql(
        &app.pool,
        &format!(
            "INSERT INTO blocked_hashes (request_id, infohash, created_at)
             SELECT request_id, '{hash}', 2 FROM fetch_jobs WHERE id = {job}",
            hash = HASH_B,
            job = jobs[0]
        ),
    )
    .await;

    let response = admin_call(&app, &admin, "GET", "/api/admin/jobs", None).await;
    assert_eq!(response.status, 200, "{}", response.body);
    let body = json_of(&response);
    assert_eq!(body["next_cursor"], Value::Null);
    let items = body["items"].as_array().unwrap();
    // Newest first.
    let ids: Vec<i64> = items.iter().map(|i| i["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![jobs[2], jobs[1], jobs[0]]);
    let newest = &items[0];
    assert_eq!(
        keys(newest),
        HashSet::from([
            "id",
            "request_id",
            "state",
            "reason",
            "infuse_pro",
            "attempts",
            "top_infohash",
            "chosen_infohash",
            "picked",
            "candidates",
            "rd_torrent_id",
            "last_error",
            "created_at",
            "updated_at",
            "title",
            "year",
            "type",
            "request_status",
            "blocked_hashes",
        ])
    );
    assert_eq!(newest["state"], "needs_pick");
    assert_eq!(newest["reason"], "no_imdb_id");
    assert_eq!(newest["title"], "Dune");
    assert_eq!(newest["year"], 2021);
    assert_eq!(newest["type"], "movie");
    assert_eq!(newest["candidates"], Value::Null);
    assert_eq!(items[1]["infuse_pro"], true);
    assert_eq!(items[1]["candidates"], Value::Null, "unparsable text");
    assert_eq!(items[2]["candidates"], json!([{"title": "A", "score": 1}]));
    assert_eq!(items[2]["top_infohash"], HASH_A);
    assert_eq!(items[2]["picked"], false);
    assert_eq!(newest["request_status"], "pending");
    assert_eq!(newest["blocked_hashes"], json!([]));
    let mut blocked: Vec<&str> = items[2]["blocked_hashes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap())
        .collect();
    blocked.sort_unstable();
    assert_eq!(blocked, vec![HASH_A, HASH_B]);

    // Filter by state.
    let queued =
        json_of(&admin_call(&app, &admin, "GET", "/api/admin/jobs?state=queued", None).await);
    let queued_ids: Vec<i64> = queued["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_i64().unwrap())
        .collect();
    assert_eq!(queued_ids, vec![jobs[1]]);
    let none =
        json_of(&admin_call(&app, &admin, "GET", "/api/admin/jobs?state=failed", None).await);
    assert!(none["items"].as_array().unwrap().is_empty());
    let all = json_of(&admin_call(&app, &admin, "GET", "/api/admin/jobs?state=all", None).await);
    assert_eq!(all["items"].as_array().unwrap().len(), 3);

    // Bad parameters.
    for query in [
        "state=bogus",
        "limit=0",
        "limit=51",
        "cursor=zzz",
        "cursor=1.x",
    ] {
        let response = admin_call(
            &app,
            &admin,
            "GET",
            &format!("/api/admin/jobs?{query}"),
            None,
        )
        .await;
        assert_error(&response, 400, "bad_request");
    }

    // One job per page visits every job once.
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let path = match &cursor {
            Some(c) => format!("/api/admin/jobs?limit=1&cursor={c}"),
            None => "/api/admin/jobs?limit=1".to_string(),
        };
        let page = json_of(&admin_call(&app, &admin, "GET", &path, None).await);
        let items = page["items"].as_array().unwrap();
        assert!(items.len() <= 1);
        seen.extend(items.iter().map(|i| i["id"].as_i64().unwrap()));
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    assert_eq!(seen, vec![jobs[2], jobs[1], jobs[0]]);
}
