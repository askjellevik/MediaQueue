//! Live updates: server-sent ticks, their access rules, session re-checks,
//! shutdown, and the limits on open streams.

mod common;

use std::time::Duration;

use common::{EventStream, TestApp, json_post_headers, login_admin_cookie, login_new_member};
use mediaqueue::events::Hub;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path_regex},
};

const SOON: Duration = Duration::from_secs(3);
const QUIET: Duration = Duration::from_millis(700);

async fn spawn_with(tmdb: &MockServer, heartbeat_ms: u64, max_streams: usize) -> TestApp {
    let uri = tmdb.uri();
    common::spawn_app_custom(
        move |cfg| cfg.tmdb_base_url = uri,
        move |state| {
            state.events = Hub::with_limits(max_streams, Duration::from_millis(heartbeat_ms));
        },
    )
    .await
}

async fn mock_movie(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/movie/\d+$"))
        .respond_with(|req: &wiremock::Request| {
            let id = req.url.path().rsplit('/').next().unwrap_or("0").to_string();
            ResponseTemplate::new(200).set_body_json(json!({
                "id": id.parse::<u32>().unwrap_or(0),
                "title": format!("Movie {id}"),
                "release_date": "2021-09-15",
                "poster_path": null,
                "imdb_id": format!("tt{id:0>7}"),
            }))
        })
        .mount(server)
        .await;
}

async fn app() -> (TestApp, MockServer) {
    let tmdb = MockServer::start().await;
    mock_movie(&tmdb).await;
    (spawn_with(&tmdb, 25_000, 200).await, tmdb)
}

async fn post(app: &TestApp, cookie: &str, path: &str, body: &str) -> common::HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("Cookie", cookie));
    app.request("POST", path, &headers, Some(body)).await
}

async fn patch_status(app: &TestApp, admin: &str, id: i64, status: &str) {
    let mut headers = json_post_headers();
    headers.push(("Cookie", admin));
    let response = app
        .request(
            "PATCH",
            &format!("/api/admin/requests/{id}"),
            &headers,
            Some(&format!(r#"{{"status":"{status}"}}"#)),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
}

#[tokio::test]
async fn streams_need_the_right_session() {
    let (app, _tmdb) = app().await;
    let member = login_new_member(&app, "Alice").await;

    assert_eq!(
        EventStream::open(app.addr, "/api/events", None)
            .await
            .status,
        401
    );
    assert_eq!(
        EventStream::open(app.addr, "/api/admin/events", None)
            .await
            .status,
        401
    );
    // Signed in, but not as admin.
    assert_eq!(
        EventStream::open(app.addr, "/api/admin/events", Some(&member))
            .await
            .status,
        403
    );
    // The admin has no member session.
    let admin = login_admin_cookie(&app).await;
    assert_eq!(
        EventStream::open(app.addr, "/api/events", Some(&admin))
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn stream_is_event_stream_and_not_cached() {
    let (app, _tmdb) = app().await;
    let member = login_new_member(&app, "Alice").await;
    let stream = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    assert_eq!(stream.status, 200);
    assert!(
        stream
            .header("content-type")
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(stream.header("cache-control"), Some("no-store"));
    assert!(stream.header("content-encoding").is_none());
}

#[tokio::test]
async fn status_change_ticks_members_and_admin() {
    let (app, _tmdb) = app().await;
    let member = login_new_member(&app, "Alice").await;
    let admin = login_admin_cookie(&app).await;
    assert_eq!(
        post(
            &app,
            &member,
            "/api/requests",
            r#"{"tmdb_id":7,"media_type":"movie"}"#
        )
        .await
        .status,
        201
    );

    let mut members = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    let mut admins = EventStream::open(app.addr, "/api/admin/events", Some(&admin)).await;
    patch_status(&app, &admin, 1, "added").await;
    assert!(
        members.saw("event: tick", SOON).await,
        "member stream got no tick"
    );
    assert!(
        admins.saw("event: tick", SOON).await,
        "admin stream got no tick"
    );

    // Setting the same status again changes nothing, so nothing is sent.
    patch_status(&app, &admin, 1, "added").await;
    assert!(!members.saw("event: tick", QUIET).await);
}

#[tokio::test]
async fn new_and_duplicate_requests_tick_only_the_admin() {
    let (app, _tmdb) = app().await;
    let alice = login_new_member(&app, "Alice").await;
    let bob = login_new_member(&app, "Bob").await;
    let admin = login_admin_cookie(&app).await;

    let mut members = EventStream::open(app.addr, "/api/events", Some(&bob)).await;
    let mut admins = EventStream::open(app.addr, "/api/admin/events", Some(&admin)).await;

    assert_eq!(
        post(
            &app,
            &alice,
            "/api/requests",
            r#"{"tmdb_id":7,"media_type":"movie"}"#
        )
        .await
        .status,
        201
    );
    assert!(
        admins.saw("event: tick", SOON).await,
        "no tick for a new request"
    );
    assert_eq!(
        post(
            &app,
            &bob,
            "/api/requests",
            r#"{"tmdb_id":7,"media_type":"movie"}"#
        )
        .await
        .status,
        200
    );
    assert!(
        admins.saw("event: tick", SOON).await,
        "no tick for a duplicate"
    );
    assert!(
        !members.saw("event: tick", QUIET).await,
        "members must not be ticked by new requests"
    );
}

#[tokio::test]
async fn stream_ends_when_the_member_is_revoked() {
    let tmdb = MockServer::start().await;
    let app = spawn_with(&tmdb, 150, 200).await;
    let member = login_new_member(&app, "Alice").await;
    let admin = login_admin_cookie(&app).await;

    let mut stream = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    assert!(stream.saw("keep-alive", SOON).await, "no heartbeat");
    assert_eq!(
        post(&app, &admin, "/api/admin/members/1/revoke", "{}")
            .await
            .status,
        200
    );
    assert!(
        stream.ended(SOON).await,
        "stream outlived the revoked member"
    );
    // And a reconnect is refused.
    assert_eq!(
        EventStream::open(app.addr, "/api/events", Some(&member))
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn stream_ends_when_the_admin_logs_out_elsewhere() {
    let tmdb = MockServer::start().await;
    let app = spawn_with(&tmdb, 150, 200).await;
    let admin = login_admin_cookie(&app).await;
    let mut stream = EventStream::open(app.addr, "/api/admin/events", Some(&admin)).await;
    assert_eq!(
        post(&app, &admin, "/api/admin/logout", "{}").await.status,
        200
    );
    assert!(
        stream.ended(SOON).await,
        "stream outlived the admin session"
    );
}

#[tokio::test]
async fn stream_outlives_the_request_timeout() {
    let tmdb = MockServer::start().await;
    let app = spawn_with(&tmdb, 1000, 200).await;
    let member = login_new_member(&app, "Alice").await;
    let admin = login_admin_cookie(&app).await;
    insert_pending(&app).await;

    let mut stream = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    // Longer than middleware::REQUEST_TIMEOUT, with heartbeats flowing.
    tokio::time::sleep(mediaqueue::middleware::REQUEST_TIMEOUT + Duration::from_secs(1)).await;
    patch_status(&app, &admin, 1, "added").await;
    assert!(stream.saw("event: tick", SOON).await, "stream was cut off");
}

async fn insert_pending(app: &TestApp) {
    common::insert_request(&app.pool, 1, 7, "movie", "pending").await;
}

#[tokio::test]
async fn open_streams_do_not_use_up_the_request_concurrency() {
    let tmdb = MockServer::start().await;
    let app = spawn_with(&tmdb, 25_000, 200).await;
    let member = login_new_member(&app, "Alice").await;
    let mut streams = Vec::new();
    for _ in 0..(mediaqueue::middleware::CONCURRENCY_LIMIT + 5) {
        streams.push(EventStream::open(app.addr, "/api/events", Some(&member)).await);
    }
    let health = app.request("GET", "/healthz", &[], None).await;
    assert_eq!(health.status, 200);
}

#[tokio::test]
async fn permits_are_released_when_a_stream_drops_and_the_cap_gives_503() {
    let tmdb = MockServer::start().await;
    let app = spawn_with(&tmdb, 25_000, 2).await;
    let member = login_new_member(&app, "Alice").await;

    let first = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    let _second = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    assert_eq!((first.status, _second.status), (200, 200));
    let refused = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    assert_eq!(refused.status, 503);

    drop(first);
    let mut status = 503;
    for _ in 0..40 {
        // The server notices the closed socket when it next writes, or on
        // the next tick; give it a heartbeat-sized nudge by publishing.
        app.state.events.publish(mediaqueue::events::Tick::Members);
        let again = EventStream::open(app.addr, "/api/events", Some(&member)).await;
        status = again.status;
        if status == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        status, 200,
        "the dropped stream's permit was never released"
    );
}

#[tokio::test]
async fn the_default_cap_is_two_hundred() {
    assert_eq!(mediaqueue::events::MAX_STREAMS, 200);
}

#[tokio::test]
async fn shutdown_ends_every_stream() {
    let (app, _tmdb) = app().await;
    let member = login_new_member(&app, "Alice").await;
    let mut stream = EventStream::open(app.addr, "/api/events", Some(&member)).await;
    app.state.events.shutdown();
    assert!(stream.ended(SOON).await);
}

#[tokio::test]
async fn counts_need_admin_and_cover_every_status() {
    let (app, _tmdb) = app().await;
    let member = login_new_member(&app, "Alice").await;
    assert_eq!(
        app.request("GET", "/api/admin/counts", &[], None)
            .await
            .status,
        401
    );
    assert_eq!(
        app.request("GET", "/api/admin/counts", &[("Cookie", &member)], None)
            .await
            .status,
        403
    );

    let admin = login_admin_cookie(&app).await;
    let empty = app
        .request("GET", "/api/admin/counts", &[("Cookie", &admin)], None)
        .await;
    let empty: Value = serde_json::from_str(&empty.body).unwrap();
    assert_eq!(
        empty,
        json!({"pending": 0, "added": 0, "already_had": 0, "refused": 0})
    );

    common::insert_request(&app.pool, 1, 1, "movie", "pending").await;
    common::insert_request(&app.pool, 1, 2, "movie", "pending").await;
    common::insert_request(&app.pool, 1, 3, "movie", "refused").await;
    let counts = app
        .request("GET", "/api/admin/counts", &[("Cookie", &admin)], None)
        .await;
    let counts: Value = serde_json::from_str(&counts.body).unwrap();
    assert_eq!(
        counts,
        json!({"pending": 2, "added": 0, "already_had": 0, "refused": 1})
    );
}
