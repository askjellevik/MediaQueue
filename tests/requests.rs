//! Phase 4 acceptance tests: `POST /api/requests`, quotas, `GET
//! /api/requests/mine` and the new-request ntfy push, against the real app
//! with wiremock TMDB and ntfy servers.

mod common;

use std::time::{Duration, Instant};

use common::{HttpResponse, TestApp, json_post_headers, login_new_member};
use mediaqueue::config::{Config, NtfyConfig};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{body_partial_json, method, path, path_regex},
};

/// Answers any `/movie/{id}` with a detail payload titled `Movie {id}`.
async fn mount_movies(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/movie/\d+$"))
        .respond_with(|req: &Request| {
            let id = req.url.path().rsplit('/').next().unwrap_or("0").to_string();
            ResponseTemplate::new(200).set_body_json(json!({
                "id": id.parse::<u32>().unwrap_or(0),
                "title": format!("Movie {id}"),
                "release_date": "2021-09-15",
                "poster_path": "/poster.jpg",
                "imdb_id": format!("tt{id:0>7}"),
            }))
        })
        .mount(server)
        .await;
}

async fn spawn_with(tmdb: &MockServer, customize: impl FnOnce(&mut Config)) -> TestApp {
    let uri = tmdb.uri();
    common::spawn_app_with(move |cfg| {
        cfg.tmdb_base_url = uri;
        customize(cfg);
    })
    .await
}

/// Members have no daily limit unless the admin sets one.
async fn set_daily_limit(app: &TestApp, name: &str, limit: i64) {
    sqlx::query("UPDATE members SET daily_limit = ? WHERE name = ?")
        .bind(limit)
        .bind(name)
        .execute(&app.pool)
        .await
        .unwrap();
}

async fn submit(app: &TestApp, cookie: &str, body: &str) -> HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("Cookie", cookie));
    app.request("POST", "/api/requests", &headers, Some(body))
        .await
}

async fn submit_movie(app: &TestApp, cookie: &str, tmdb_id: u32) -> HttpResponse {
    submit(
        app,
        cookie,
        &format!(r#"{{"tmdb_id":{tmdb_id},"media_type":"movie"}}"#),
    )
    .await
}

fn json_of(response: &HttpResponse) -> Value {
    serde_json::from_str(&response.body).unwrap_or_else(|_| panic!("not json: {}", response.body))
}

async fn count(app: &TestApp, table: &str) -> i64 {
    // Table names are fixed strings from this file, never user input.
    let sql = match table {
        "requests" => "SELECT COUNT(*) FROM requests",
        "request_events" => "SELECT COUNT(*) FROM request_events",
        other => panic!("unexpected table {other}"),
    };
    sqlx::query_scalar(sql).fetch_one(&app.pool).await.unwrap()
}

// ---------------------------------------------------------------------
// New and duplicate
// ---------------------------------------------------------------------

#[tokio::test]
async fn two_members_requesting_one_title_make_one_row_and_two_events() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;
    let bob = login_new_member(&app, "Bob").await;
    set_daily_limit(&app, "Alice", 10).await;
    set_daily_limit(&app, "Bob", 10).await;

    let first = submit_movie(&app, &alice, 438631).await;
    assert_eq!(first.status, 201, "{}", first.body);
    let first = json_of(&first);
    assert_eq!(first["created"], true);
    assert_eq!(first["request"]["title"], "Movie 438631");
    assert_eq!(first["request"]["status"], "pending");
    assert_eq!(first["remaining_today"], 9);

    let second = submit_movie(&app, &bob, 438631).await;
    assert_eq!(second.status, 200, "{}", second.body);
    let second = json_of(&second);
    assert_eq!(second["created"], false);
    assert_eq!(second["request"]["id"], first["request"]["id"]);
    // A duplicate does not use up the daily quota.
    assert_eq!(second["remaining_today"], 10);

    assert_eq!(count(&app, "requests").await, 1);
    let events: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, actor FROM request_events ORDER BY id")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        events,
        [
            ("submitted".to_string(), "Alice".to_string()),
            ("duplicate".to_string(), "Bob".to_string())
        ]
    );

    // The row holds TMDB's data, fetched server-side.
    let (title, year, imdb, poster, requested_by): (String, i64, String, String, i64) =
        sqlx::query_as(
            "SELECT r.title, r.year, r.imdb_id, r.poster_path, r.requested_by FROM requests r",
        )
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        (title.as_str(), year, imdb.as_str(), poster.as_str()),
        ("Movie 438631", 2021, "tt0438631", "/poster.jpg")
    );
    let alice_id: i64 = sqlx::query_scalar("SELECT id FROM members WHERE name = 'Alice'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(requested_by, alice_id);
}

#[tokio::test]
async fn note_is_stored_cleaned() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;

    let body = r#"{"tmdb_id":1,"media_type":"movie","note":"  4K\u0000 please\n "}"#;
    assert_eq!(submit(&app, &alice, body).await.status, 201);
    let (row_note, event_note): (String, String) = sqlx::query_as(
        "SELECT r.note, e.note FROM requests r JOIN request_events e ON e.request_id = r.id",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(row_note, "4K please");
    assert_eq!(event_note, "4K please");
}

// ---------------------------------------------------------------------
// Quotas
// ---------------------------------------------------------------------

#[tokio::test]
async fn eleventh_request_in_a_day_is_429_and_writes_nothing() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;
    set_daily_limit(&app, "Alice", 10).await;

    for id in 1..=10 {
        let response = submit_movie(&app, &alice, id).await;
        assert_eq!(response.status, 201, "request {id}: {}", response.body);
        assert_eq!(json_of(&response)["remaining_today"], 10 - id);
        // Spaced out so the per-minute cap never triggers.
        app.clock.advance(Duration::from_secs(30));
    }
    let tmdb_calls = tmdb.received_requests().await.unwrap().len();
    let (rows, events) = (
        count(&app, "requests").await,
        count(&app, "request_events").await,
    );

    let over = submit_movie(&app, &alice, 11).await;
    assert_eq!(over.status, 429, "{}", over.body);
    assert_eq!(
        json_of(&over),
        json!({
            "error": "quota_exceeded",
            "message": "you have used up your requests for now",
            "remaining_today": 0
        })
    );
    // Retry-After points at the next UTC midnight.
    let now = app.clock.unix_secs();
    let until_midnight = 86_400 - now % 86_400;
    assert_eq!(
        over.header("retry-after"),
        Some(until_midnight.to_string().as_str())
    );

    // Nothing written, and TMDB was not even asked.
    assert_eq!(count(&app, "requests").await, rows);
    assert_eq!(count(&app, "request_events").await, events);
    assert_eq!(tmdb.received_requests().await.unwrap().len(), tmdb_calls);

    // /api/me agrees, and a new day resets it.
    let me = app
        .request("GET", "/api/me", &[("Cookie", &alice)], None)
        .await;
    assert_eq!(json_of(&me)["remaining_today"], 0);
    app.clock.advance(Duration::from_secs(until_midnight));
    assert_eq!(submit_movie(&app, &alice, 11).await.status, 201);
}

#[tokio::test]
async fn member_without_a_daily_limit_is_never_daily_capped() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;

    for id in 1..=15 {
        let response = submit_movie(&app, &alice, id).await;
        assert_eq!(response.status, 201, "request {id}: {}", response.body);
        assert!(json_of(&response)["remaining_today"].is_null());
        app.clock.advance(Duration::from_secs(30));
    }
    let me = app
        .request("GET", "/api/me", &[("Cookie", &alice)], None)
        .await;
    assert!(json_of(&me)["remaining_today"].is_null());
}

#[tokio::test]
async fn unlimited_member_still_hits_the_minute_cap_without_a_remaining_count() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |cfg| cfg.rate_limits.requests_per_minute = 2).await;
    let alice = login_new_member(&app, "Alice").await;

    assert_eq!(submit_movie(&app, &alice, 1).await.status, 201);
    assert_eq!(submit_movie(&app, &alice, 2).await.status, 201);
    let over = submit_movie(&app, &alice, 3).await;
    assert_eq!(over.status, 429);
    assert!(json_of(&over).get("remaining_today").is_none());
}

#[tokio::test]
async fn limited_member_sees_the_count_go_down() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;
    set_daily_limit(&app, "Alice", 5).await;

    let me = app
        .request("GET", "/api/me", &[("Cookie", &alice)], None)
        .await;
    assert_eq!(json_of(&me)["remaining_today"], 5);
    assert_eq!(submit_movie(&app, &alice, 1).await.status, 201);
    let me = app
        .request("GET", "/api/me", &[("Cookie", &alice)], None)
        .await;
    assert_eq!(json_of(&me)["remaining_today"], 4);
}

#[tokio::test]
async fn member_daily_limit_caps_requests() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;
    set_daily_limit(&app, "Alice", 1).await;

    assert_eq!(submit_movie(&app, &alice, 1).await.status, 201);
    app.clock.advance(Duration::from_secs(61));
    assert_eq!(submit_movie(&app, &alice, 2).await.status, 429);
}

#[tokio::test]
async fn fourth_request_in_a_minute_is_429_until_the_oldest_ages_out() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |cfg| cfg.rate_limits.requests_per_minute = 3).await;
    let alice = login_new_member(&app, "Alice").await;
    set_daily_limit(&app, "Alice", 10).await;

    assert_eq!(submit_movie(&app, &alice, 1).await.status, 201);
    app.clock.advance(Duration::from_secs(10));
    assert_eq!(submit_movie(&app, &alice, 2).await.status, 201);
    // Duplicates count toward the per-minute cap too.
    assert_eq!(submit_movie(&app, &alice, 2).await.status, 200);

    let over = submit_movie(&app, &alice, 3).await;
    assert_eq!(over.status, 429);
    assert_eq!(over.header("retry-after"), Some("50"));
    assert_eq!(json_of(&over)["remaining_today"], 8);

    app.clock.advance(Duration::from_secs(50));
    assert_eq!(submit_movie(&app, &alice, 3).await.status, 201);
}

// ---------------------------------------------------------------------
// TMDB failures and bad input
// ---------------------------------------------------------------------

#[tokio::test]
async fn tmdb_failure_during_submit_is_a_clean_error_and_writes_nothing() {
    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&tmdb)
        .await;
    Mock::given(method("GET"))
        .and(path("/movie/2"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"status_code": 34})))
        .mount(&tmdb)
        .await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;

    let down = submit_movie(&app, &alice, 1).await;
    assert_eq!(down.status, 503);
    assert_eq!(json_of(&down)["error"], "upstream_unavailable");

    let missing = submit_movie(&app, &alice, 2).await;
    assert_eq!(missing.status, 404);
    assert_eq!(
        missing.body,
        r#"{"error":"not_found","message":"not found"}"#
    );

    assert_eq!(count(&app, "requests").await, 0);
    assert_eq!(count(&app, "request_events").await, 0);
}

#[tokio::test]
async fn malformed_bodies_are_rejected_without_touching_tmdb() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;

    let long_note = "x".repeat(201);
    for body in [
        r#"{"tmdb_id":0,"media_type":"movie"}"#.to_string(),
        r#"{"tmdb_id":-5,"media_type":"movie"}"#.to_string(),
        r#"{"tmdb_id":4294967296,"media_type":"movie"}"#.to_string(),
        r#"{"tmdb_id":"12","media_type":"movie"}"#.to_string(),
        r#"{"tmdb_id":12,"media_type":"person"}"#.to_string(),
        r#"{"tmdb_id":12}"#.to_string(),
        // Client-sent titles are refused outright.
        r#"{"tmdb_id":12,"media_type":"movie","title":"Fake"}"#.to_string(),
        format!(r#"{{"tmdb_id":12,"media_type":"movie","note":"{long_note}"}}"#),
        "not json".to_string(),
    ] {
        let response = submit(&app, &alice, &body).await;
        assert_eq!(response.status, 400, "{body}: {}", response.body);
    }
    assert!(tmdb.received_requests().await.unwrap().is_empty());
    assert_eq!(count(&app, "requests").await, 0);
}

#[tokio::test]
async fn submitting_needs_a_session_and_the_csrf_headers() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |_| {}).await;
    let alice = login_new_member(&app, "Alice").await;
    let body = r#"{"tmdb_id":1,"media_type":"movie"}"#;

    let anonymous = app
        .request("POST", "/api/requests", &json_post_headers(), Some(body))
        .await;
    assert_eq!(anonymous.status, 401);

    let no_csrf = app
        .request(
            "POST",
            "/api/requests",
            &[("Cookie", &alice), ("Content-Type", "application/json")],
            Some(body),
        )
        .await;
    assert_eq!(no_csrf.status, 403);
    assert_eq!(count(&app, "requests").await, 0);
}

// ---------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------

#[tokio::test]
async fn twenty_concurrent_identical_submits_make_exactly_one_row() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |cfg| {
        cfg.rate_limits.requests_per_minute = 100;
    })
    .await;
    let alice = login_new_member(&app, "Alice").await;

    let handles: Vec<_> = (0..20)
        .map(|_| {
            let addr = app.addr;
            let cookie = alice.clone();
            tokio::spawn(async move {
                let mut headers = json_post_headers();
                headers.push(("Cookie", &cookie));
                common::http_request(
                    addr,
                    "POST",
                    "/api/requests",
                    &headers,
                    Some(r#"{"tmdb_id":42,"media_type":"movie"}"#),
                )
                .await
            })
        })
        .collect();
    let mut statuses = Vec::new();
    for handle in handles {
        statuses.push(handle.await.unwrap().status);
    }
    statuses.sort();

    assert_eq!(
        statuses.iter().filter(|s| **s == 201).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses.iter().filter(|s| **s == 200).count(),
        19,
        "{statuses:?}"
    );
    assert_eq!(count(&app, "requests").await, 1);
    assert_eq!(count(&app, "request_events").await, 20);
}

#[tokio::test]
async fn concurrent_submits_never_overshoot_the_minute_cap() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |cfg| cfg.rate_limits.requests_per_minute = 3).await;
    let alice = login_new_member(&app, "Alice").await;

    let handles: Vec<_> = (1..=10)
        .map(|id| {
            let addr = app.addr;
            let cookie = alice.clone();
            tokio::spawn(async move {
                let mut headers = json_post_headers();
                headers.push(("Cookie", &cookie));
                let body = format!(r#"{{"tmdb_id":{id},"media_type":"movie"}}"#);
                common::http_request(addr, "POST", "/api/requests", &headers, Some(&body)).await
            })
        })
        .collect();
    let mut statuses = Vec::new();
    for handle in handles {
        statuses.push(handle.await.unwrap().status);
    }
    assert!(
        statuses.iter().all(|s| *s == 201 || *s == 429),
        "{statuses:?}"
    );
    assert_eq!(statuses.iter().filter(|s| **s == 201).count(), 3);
    assert_eq!(count(&app, "requests").await, 3);
}

// ---------------------------------------------------------------------
// My requests
// ---------------------------------------------------------------------

#[tokio::test]
async fn my_requests_pages_through_own_titles_newest_first() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let app = spawn_with(&tmdb, |cfg| cfg.rate_limits.requests_per_minute = 100).await;
    let alice = login_new_member(&app, "Alice").await;
    let bob = login_new_member(&app, "Bob").await;

    for id in 1..=4 {
        assert_eq!(submit_movie(&app, &alice, id).await.status, 201);
        app.clock.advance(Duration::from_secs(5));
    }
    // Bob's own title, and one Alice later asks for too.
    assert_eq!(submit_movie(&app, &bob, 50).await.status, 201);
    assert_eq!(submit_movie(&app, &bob, 51).await.status, 201);
    assert_eq!(submit_movie(&app, &alice, 51).await.status, 200);
    sqlx::query("UPDATE requests SET status = 'added' WHERE tmdb_id = 2")
        .execute(&app.pool)
        .await
        .unwrap();

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let uri = match &cursor {
            None => "/api/requests/mine?limit=2".to_string(),
            Some(c) => format!("/api/requests/mine?limit=2&cursor={c}"),
        };
        let page = app.request("GET", &uri, &[("Cookie", &alice)], None).await;
        assert_eq!(page.status, 200, "{}", page.body);
        let page = json_of(&page);
        for item in page["items"].as_array().unwrap() {
            seen.push((
                item["tmdb_id"].as_i64().unwrap(),
                item["status"].as_str().unwrap().to_string(),
            ));
        }
        pages += 1;
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let ids: Vec<i64> = seen.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [51, 4, 3, 2, 1]);
    assert_eq!(seen[3].1, "added");
    assert!(!ids.contains(&50), "another member's request leaked");

    let bad = app
        .request(
            "GET",
            "/api/requests/mine?cursor=nope",
            &[("Cookie", &alice)],
            None,
        )
        .await;
    assert_eq!(bad.status, 400);
}

// ---------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------

async fn spawn_with_ntfy(tmdb: &MockServer, ntfy: &MockServer) -> TestApp {
    let ntfy_uri = ntfy.uri();
    spawn_with(tmdb, move |cfg| {
        cfg.ntfy = NtfyConfig {
            url: Some(ntfy_uri),
            topic: "requests-topic".into(),
            token: None,
        };
    })
    .await
}

async fn wait_for_pushes(ntfy: &MockServer, n: usize) -> Vec<Value> {
    for _ in 0..200 {
        let received = ntfy.received_requests().await.unwrap_or_default();
        if received.len() >= n {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    ntfy.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn new_titles_push_one_notification_and_duplicates_none() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(body_partial_json(json!({
            "topic": "requests-topic",
            "title": "New request: Movie 7 (2021)",
            "message": "Movie, requested by Alice\nNote: please\nIMDb: tt0000007",
            "tags": ["movie_camera"],
            "click": "https://example.com/admin#pending",
        })))
        .respond_with(ResponseTemplate::new(200))
        .mount(&ntfy)
        .await;
    let app = spawn_with_ntfy(&tmdb, &ntfy).await;
    let alice = login_new_member(&app, "Alice").await;
    let bob = login_new_member(&app, "Bob").await;

    let body = r#"{"tmdb_id":7,"media_type":"movie","note":"please"}"#;
    assert_eq!(submit(&app, &alice, body).await.status, 201);
    assert_eq!(submit_movie(&app, &bob, 7).await.status, 200);

    let pushes = wait_for_pushes(&ntfy, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pushes.len(), 1);
    assert_eq!(ntfy.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn slow_or_failing_ntfy_never_delays_or_fails_the_request() {
    let tmdb = MockServer::start().await;
    mount_movies(&tmdb).await;
    let ntfy = MockServer::start().await;
    // First push hangs past the 3 s send timeout; the rest fail.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(10)))
        .up_to_n_times(1)
        .mount(&ntfy)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&ntfy)
        .await;
    let app = spawn_with_ntfy(&tmdb, &ntfy).await;
    let alice = login_new_member(&app, "Alice").await;

    for id in 1..=2 {
        let started = Instant::now();
        let response = submit_movie(&app, &alice, id).await;
        assert_eq!(response.status, 201, "{}", response.body);
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "request took {:?}",
            started.elapsed()
        );
    }
    assert_eq!(wait_for_pushes(&ntfy, 2).await.len(), 2);
    // The app is still healthy afterwards.
    assert_eq!(app.request("GET", "/healthz", &[], None).await.status, 200);
}
