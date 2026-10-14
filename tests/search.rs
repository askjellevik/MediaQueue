//! Phase 3 acceptance tests: `/api/search` and the TMDB caches, against
//! the real app with a wiremock TMDB. Cache time comes from the app's fake
//! clock; nothing here sleeps to move time forward.

mod common;

use std::{collections::BTreeSet, time::Duration};

use common::{HttpResponse, TEST_TMDB_TOKEN, TestApp, insert_request, login_new_member};
use media_requests::tmdb::{MediaType, TmdbError};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

const ELEVEN_MINUTES: Duration = Duration::from_secs(11 * 60);

async fn tmdb_mock() -> MockServer {
    MockServer::start().await
}

/// A `/search/multi` payload with one movie per title.
fn multi_body(titles: &[&str]) -> Value {
    let results: Vec<Value> = titles
        .iter()
        .enumerate()
        .map(|(i, title)| {
            json!({
                "id": 100 + i,
                "media_type": "movie",
                "title": title,
                "original_title": title,
                "release_date": "2021-09-15",
                "poster_path": "/poster.jpg",
                "backdrop_path": "/backdrop.jpg",
                "overview": "A film.",
                "popularity": 99.5,
                "vote_average": 7.8,
                "genre_ids": [1, 2],
            })
        })
        .collect();
    json!({ "page": 1, "results": results, "total_pages": 1, "total_results": titles.len() })
}

/// Mounts a `/search/multi` mock answering `status` with `body`, after
/// `delay` (so concurrent callers overlap on one in-flight call).
async fn mount_search(server: &MockServer, status: u16, body: Value, delay: Duration) {
    Mock::given(method("GET"))
        .and(path("/search/multi"))
        .and(header("authorization", format!("Bearer {TEST_TMDB_TOKEN}")))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_json(body)
                .set_delay(delay),
        )
        .mount(server)
        .await;
}

async fn upstream_calls(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

async fn spawn(server: &MockServer) -> TestApp {
    let uri = server.uri();
    common::spawn_app_with(move |cfg| {
        cfg.tmdb_base_url = uri;
        // Concurrency tests fire 20 searches from one member at once.
        cfg.rate_limits.search_burst = 1000;
    })
    .await
}

async fn search(app: &TestApp, cookie: &str, query: &str) -> HttpResponse {
    let uri = format!("/api/search?q={query}");
    app.request("GET", &uri, &[("Cookie", cookie)], None).await
}

fn titles(response: &HttpResponse) -> Vec<String> {
    let body: Value = serde_json::from_str(&response.body).expect("json body");
    body["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| r["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Fires `n` identical searches at once and returns every response.
async fn concurrent_searches(
    app: &TestApp,
    cookie: &str,
    query: &str,
    n: usize,
) -> Vec<HttpResponse> {
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let addr = app.addr;
            let cookie = cookie.to_string();
            let uri = format!("/api/search?q={query}");
            tokio::spawn(async move {
                common::http_request(addr, "GET", &uri, &[("Cookie", &cookie)], None).await
            })
        })
        .collect();
    let mut responses = Vec::new();
    for handle in handles {
        responses.push(handle.await.expect("search task"));
    }
    responses
}

// ---------------------------------------------------------------------
// Single flight and the fresh state
// ---------------------------------------------------------------------

#[tokio::test]
async fn twenty_concurrent_identical_searches_make_one_upstream_call() {
    let server = tmdb_mock().await;
    mount_search(
        &server,
        200,
        multi_body(&["Dune"]),
        Duration::from_millis(300),
    )
    .await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;

    for response in concurrent_searches(&app, &cookie, "dune", 20).await {
        assert_eq!(response.status, 200, "{}", response.body);
        assert_eq!(titles(&response), ["Dune"]);
    }
    assert_eq!(upstream_calls(&server).await, 1);
}

#[tokio::test]
async fn repeat_search_in_the_same_epoch_is_served_fresh() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Dune"]), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;

    assert_eq!(search(&app, &cookie, "dune").await.status, 200);
    app.clock.advance(Duration::from_secs(60));
    // Same normalised query: case and extra spaces don't matter.
    let again = search(&app, &cookie, "%20DUNE%20%20").await;
    assert_eq!(again.status, 200);
    assert_eq!(titles(&again), ["Dune"]);
    assert_eq!(upstream_calls(&server).await, 1);
}

// ---------------------------------------------------------------------
// Stale-usable: refresh succeeds or fails
// ---------------------------------------------------------------------

#[tokio::test]
async fn successful_refresh_replaces_the_stale_entry_with_one_call() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Old"]), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;
    assert_eq!(titles(&search(&app, &cookie, "dune").await), ["Old"]);

    app.clock.advance(ELEVEN_MINUTES);
    server.reset().await;
    mount_search(
        &server,
        200,
        multi_body(&["New"]),
        Duration::from_millis(300),
    )
    .await;

    for response in concurrent_searches(&app, &cookie, "dune", 20).await {
        assert_eq!(response.status, 200);
        assert_eq!(titles(&response), ["New"]);
    }
    assert_eq!(upstream_calls(&server).await, 1);

    // The refreshed entry is now the fresh one.
    app.clock.advance(Duration::from_secs(60));
    assert_eq!(titles(&search(&app, &cookie, "dune").await), ["New"]);
    assert_eq!(upstream_calls(&server).await, 1);
}

#[tokio::test]
async fn failed_refresh_serves_stale_and_backs_off_for_30_seconds() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Dune"]), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;
    assert_eq!(search(&app, &cookie, "dune").await.status, 200);

    app.clock.advance(ELEVEN_MINUTES);
    server.reset().await;
    mount_search(&server, 500, json!({}), Duration::ZERO).await;

    let stale = search(&app, &cookie, "dune").await;
    assert_eq!(stale.status, 200);
    assert_eq!(titles(&stale), ["Dune"]);
    assert_eq!(upstream_calls(&server).await, 1);

    // Inside the unhealthy window: stale, and TMDB is left alone.
    app.clock.advance(Duration::from_secs(20));
    assert_eq!(titles(&search(&app, &cookie, "dune").await), ["Dune"]);
    assert_eq!(upstream_calls(&server).await, 1);

    // Past it: one more refresh attempt, still failing, still stale.
    app.clock.advance(Duration::from_secs(11));
    assert_eq!(titles(&search(&app, &cookie, "dune").await), ["Dune"]);
    assert_eq!(upstream_calls(&server).await, 2);
}

#[tokio::test]
async fn refresh_that_times_out_serves_stale() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Dune"]), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;
    assert_eq!(search(&app, &cookie, "dune").await.status, 200);

    app.clock.advance(ELEVEN_MINUTES);
    server.reset().await;
    // Longer than the 3 s upstream timeout (this test waits that out).
    mount_search(&server, 200, multi_body(&["Late"]), Duration::from_secs(5)).await;

    let stale = search(&app, &cookie, "dune").await;
    assert_eq!(stale.status, 200);
    assert_eq!(titles(&stale), ["Dune"]);

    app.clock.advance(Duration::from_secs(10));
    assert_eq!(titles(&search(&app, &cookie, "dune").await), ["Dune"]);
    assert_eq!(upstream_calls(&server).await, 1);
}

#[tokio::test]
async fn tmdb_429_counts_as_a_failed_refresh() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Dune"]), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;
    assert_eq!(search(&app, &cookie, "dune").await.status, 200);

    app.clock.advance(ELEVEN_MINUTES);
    server.reset().await;
    mount_search(&server, 429, json!({}), Duration::ZERO).await;
    assert_eq!(titles(&search(&app, &cookie, "dune").await), ["Dune"]);
}

// ---------------------------------------------------------------------
// Gone: past 60 minutes
// ---------------------------------------------------------------------

#[tokio::test]
async fn entry_older_than_an_hour_is_never_served() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Old"]), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;
    assert_eq!(search(&app, &cookie, "dune").await.status, 200);

    app.clock.advance(Duration::from_secs(61 * 60));
    server.reset().await;
    mount_search(&server, 500, json!({}), Duration::ZERO).await;

    let gone = search(&app, &cookie, "dune").await;
    assert_eq!(gone.status, 503, "{}", gone.body);
    let body: Value = serde_json::from_str(&gone.body).unwrap();
    assert_eq!(body["error"], "upstream_unavailable");
    assert!(!gone.body.contains("Old"));

    // TMDB back: a plain miss and refill, even right after the failure.
    server.reset().await;
    mount_search(&server, 200, multi_body(&["New"]), Duration::ZERO).await;
    let refilled = search(&app, &cookie, "dune").await;
    assert_eq!(refilled.status, 200);
    assert_eq!(titles(&refilled), ["New"]);
    assert_eq!(upstream_calls(&server).await, 1);
}

#[tokio::test]
async fn first_search_with_tmdb_down_is_503() {
    let server = tmdb_mock().await;
    mount_search(&server, 502, json!({}), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;
    assert_eq!(search(&app, &cookie, "dune").await.status, 503);
}

// ---------------------------------------------------------------------
// Payload shape and state annotation
// ---------------------------------------------------------------------

#[tokio::test]
async fn people_are_dropped_and_payload_is_slim_and_capped_at_eight() {
    let server = tmdb_mock().await;
    let mut results = vec![json!({
        "id": 1, "media_type": "person", "name": "Some Actor",
        "known_for": [], "profile_path": "/face.jpg"
    })];
    for id in 2..14 {
        results.push(json!({
            "id": id,
            "media_type": if id % 2 == 0 { "movie" } else { "tv" },
            "title": format!("Movie {id}"),
            "name": format!("Show {id}"),
            "release_date": "2020-05-01",
            "first_air_date": "2018-01-01",
            "poster_path": "/p.jpg",
            "overview": "o".repeat(1000),
            "popularity": 1.0,
        }));
    }
    mount_search(&server, 200, json!({ "results": results }), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;

    let response = search(&app, &cookie, "something").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("cache-control"), Some("no-store"));
    let body: Value = serde_json::from_str(&response.body).unwrap();
    let items = body["results"].as_array().unwrap();
    assert_eq!(items.len(), 8);

    let expected: BTreeSet<&str> = [
        "tmdb_id", "type", "title", "year", "poster", "overview", "state",
    ]
    .into_iter()
    .collect();
    for item in items {
        let keys: BTreeSet<&str> = item
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, expected);
        assert_ne!(item["tmdb_id"], 1, "person result leaked through");
        assert!(item["overview"].as_str().unwrap().chars().count() <= 301);
    }
    assert_eq!(items[0]["type"], "movie");
    assert_eq!(items[0]["title"], "Movie 2");
    assert_eq!(items[0]["year"], 2020);
    assert_eq!(items[0]["poster"], "/p.jpg");
    assert_eq!(items[1]["type"], "tv");
    assert_eq!(items[1]["title"], "Show 3");
    assert_eq!(items[1]["year"], 2018);
}

#[tokio::test]
async fn type_filter_keeps_only_that_type() {
    let server = tmdb_mock().await;
    let results = json!({ "results": [
        { "id": 1, "media_type": "movie", "title": "A Movie" },
        { "id": 2, "media_type": "tv", "name": "A Show" },
    ]});
    mount_search(&server, 200, results, Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;

    let tv = app
        .request(
            "GET",
            "/api/search?q=show&type=tv",
            &[("Cookie", &cookie)],
            None,
        )
        .await;
    assert_eq!(titles(&tv), ["A Show"]);
    let movie = app
        .request(
            "GET",
            "/api/search?q=show&type=movie",
            &[("Cookie", &cookie)],
            None,
        )
        .await;
    assert_eq!(titles(&movie), ["A Movie"]);
}

#[tokio::test]
async fn results_are_annotated_with_request_state() {
    let server = tmdb_mock().await;
    let results = json!({ "results": [
        { "id": 10, "media_type": "movie", "title": "Pending" },
        { "id": 11, "media_type": "movie", "title": "Added" },
        { "id": 12, "media_type": "movie", "title": "Had" },
        { "id": 13, "media_type": "movie", "title": "Refused" },
        { "id": 14, "media_type": "movie", "title": "Untouched" },
        // Same TMDB id as a requested movie, but a series: not requested.
        { "id": 10, "media_type": "tv", "name": "Other type" },
    ]});
    mount_search(&server, 200, results, Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;
    let (bob, _) = common::create_member(&app.pool, &app.config, "Bob").await;
    insert_request(&app.pool, bob, 10, "movie", "pending").await;
    insert_request(&app.pool, bob, 11, "movie", "added").await;
    insert_request(&app.pool, bob, 12, "movie", "already_had").await;
    insert_request(&app.pool, bob, 13, "movie", "refused").await;

    let response = search(&app, &cookie, "anything").await;
    let body: Value = serde_json::from_str(&response.body).unwrap();
    let states: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["state"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        ["requested", "added", "have", "unavailable", "none", "none"]
    );
    // Who asked is never part of a search result.
    assert!(!response.body.contains("Bob"));
}

// ---------------------------------------------------------------------
// Validation, auth and limits
// ---------------------------------------------------------------------

#[tokio::test]
async fn search_requires_a_session() {
    let app = common::spawn_app().await;
    let response = app.request("GET", "/api/search?q=dune", &[], None).await;
    assert_eq!(response.status, 401);
}

#[tokio::test]
async fn bad_queries_are_rejected_before_any_upstream_call() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Dune"]), Duration::ZERO).await;
    let app = spawn(&server).await;
    let cookie = login_new_member(&app, "Alice").await;

    let long = "a".repeat(81);
    for uri in [
        "/api/search".to_string(),
        "/api/search?q=a".to_string(),
        "/api/search?q=%20%20a%20%20".to_string(),
        format!("/api/search?q={long}"),
        "/api/search?q=dune&type=person".to_string(),
    ] {
        let response = app.request("GET", &uri, &[("Cookie", &cookie)], None).await;
        assert_eq!(response.status, 400, "{uri}");
        assert_eq!(
            response.body,
            r#"{"error":"bad_request","message":"the request was malformed"}"#
        );
    }
    let max = "a".repeat(80);
    assert_eq!(search(&app, &cookie, &max).await.status, 200);
    assert_eq!(upstream_calls(&server).await, 1);
}

#[tokio::test]
async fn search_is_rate_limited_per_member() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Dune"]), Duration::ZERO).await;
    let app = common::spawn_app_with({
        let uri = server.uri();
        move |cfg| cfg.tmdb_base_url = uri
    })
    .await;
    let alice = login_new_member(&app, "Alice").await;
    let bob = login_new_member(&app, "Bob").await;

    for _ in 0..10 {
        assert_eq!(search(&app, &alice, "dune").await.status, 200);
    }
    let limited = search(&app, &alice, "dune").await;
    assert_eq!(limited.status, 429);
    assert_eq!(limited.header("retry-after"), Some("1"));
    // Another member is unaffected.
    assert_eq!(search(&app, &bob, "dune").await.status, 200);
    // Refill: 5 per second.
    app.clock.advance(Duration::from_millis(200));
    assert_eq!(search(&app, &alice, "dune").await.status, 200);
}

// ---------------------------------------------------------------------
// Detail lookups
// ---------------------------------------------------------------------

async fn mount_detail(server: &MockServer, url_path: &str, status: u16, body: Value) {
    Mock::given(method("GET"))
        .and(path(url_path))
        .and(header("authorization", format!("Bearer {TEST_TMDB_TOKEN}")))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

fn movie_detail() -> Value {
    json!({
        "id": 438631, "title": "Dune", "release_date": "2021-09-15",
        "poster_path": "/d5NXSklXo0qyIYkgV94XAgMIckC.jpg", "imdb_id": "tt1160419",
        "runtime": 155, "budget": 165000000
    })
}

#[tokio::test]
async fn movie_and_tv_details_include_imdb_ids() {
    let server = tmdb_mock().await;
    mount_detail(&server, "/movie/438631", 200, movie_detail()).await;
    Mock::given(method("GET"))
        .and(path("/tv/1399"))
        .and(query_param("append_to_response", "external_ids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1399, "name": "Game of Thrones", "first_air_date": "2011-04-17",
            "poster_path": "/got.jpg",
            "external_ids": { "imdb_id": "tt0944947", "tvdb_id": 121361 }
        })))
        .mount(&server)
        .await;
    let app = spawn(&server).await;

    let movie = app
        .state
        .tmdb
        .detail(438631, MediaType::Movie)
        .await
        .unwrap();
    assert_eq!(movie.title, "Dune");
    assert_eq!(movie.year, Some(2021));
    assert_eq!(movie.imdb_id.as_deref(), Some("tt1160419"));

    let tv = app.state.tmdb.detail(1399, MediaType::Tv).await.unwrap();
    assert_eq!(tv.title, "Game of Thrones");
    assert_eq!(tv.year, Some(2011));
    assert_eq!(tv.poster_path.as_deref(), Some("/got.jpg"));
    assert_eq!(tv.imdb_id.as_deref(), Some("tt0944947"));

    // Cached for the day.
    app.state
        .tmdb
        .detail(438631, MediaType::Movie)
        .await
        .unwrap();
    assert_eq!(upstream_calls(&server).await, 2);
}

#[tokio::test]
async fn stale_detail_is_served_while_tmdb_is_down_for_up_to_a_week() {
    let server = tmdb_mock().await;
    mount_detail(&server, "/movie/438631", 200, movie_detail()).await;
    let app = spawn(&server).await;
    app.state
        .tmdb
        .detail(438631, MediaType::Movie)
        .await
        .unwrap();

    server.reset().await;
    mount_detail(&server, "/movie/438631", 503, json!({})).await;

    app.clock.advance(Duration::from_secs(3 * 86_400));
    let stale = app
        .state
        .tmdb
        .detail(438631, MediaType::Movie)
        .await
        .unwrap();
    assert_eq!(stale.title, "Dune");

    app.clock.advance(Duration::from_secs(5 * 86_400));
    let gone = app.state.tmdb.detail(438631, MediaType::Movie).await;
    assert_eq!(gone, Err(TmdbError::Unavailable));
}

#[tokio::test]
async fn detail_404_is_not_found_even_with_a_stale_entry_and_removes_it() {
    let server = tmdb_mock().await;
    mount_detail(&server, "/movie/438631", 200, movie_detail()).await;
    let app = spawn(&server).await;
    app.state
        .tmdb
        .detail(438631, MediaType::Movie)
        .await
        .unwrap();

    app.clock.advance(Duration::from_secs(86_400 + 60));
    server.reset().await;
    mount_detail(
        &server,
        "/movie/438631",
        404,
        json!({ "success": false, "status_code": 34 }),
    )
    .await;
    let missing = app.state.tmdb.detail(438631, MediaType::Movie).await;
    assert_eq!(missing, Err(TmdbError::NotFound));

    // The stale entry is gone: with TMDB now down there is nothing to
    // fall back on.
    server.reset().await;
    mount_detail(&server, "/movie/438631", 500, json!({})).await;
    app.clock.advance(Duration::from_secs(31));
    let after = app.state.tmdb.detail(438631, MediaType::Movie).await;
    assert_eq!(after, Err(TmdbError::Unavailable));
}

#[tokio::test]
async fn global_tmdb_limit_answers_503_without_calling_upstream() {
    let server = tmdb_mock().await;
    mount_search(&server, 200, multi_body(&["Dune"]), Duration::ZERO).await;
    let uri = server.uri();
    let app = common::spawn_app_with(move |cfg| {
        cfg.tmdb_base_url = uri;
        cfg.rate_limits.tmdb_per_second = 1;
    })
    .await;
    let cookie = login_new_member(&app, "Alice").await;

    assert_eq!(search(&app, &cookie, "dune").await.status, 200);
    // A different query needs TMDB, but this second's call is used up.
    let limited = search(&app, &cookie, "alien").await;
    assert_eq!(limited.status, 503);
    assert_eq!(upstream_calls(&server).await, 1);
    // Cached results are unaffected, and the next second refills.
    assert_eq!(search(&app, &cookie, "dune").await.status, 200);
    app.clock.advance(Duration::from_secs(1));
    assert_eq!(search(&app, &cookie, "alien").await.status, 200);
    assert_eq!(upstream_calls(&server).await, 2);
}
