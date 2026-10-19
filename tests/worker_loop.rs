//! The worker's loop against the real portal (public and internal
//! listeners) and a stub Torrentio. Each case drives `run_once`, so nothing
//! sleeps. The fixture is invented; see `worker_golden.rs`.

mod common;

use std::time::{Duration, Instant};

use common::{TEST_INTERNAL_TOKEN, TestApp, json_post_headers, login_admin_cookie};
use mediaqueue::worker::{
    portal::PortalClient,
    run::{Cycle, Shutdown, Timing, Worker},
    settings::RankSettings,
    torrentio::TorrentioSource,
};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::watch;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const FIXTURE: &str = include_str!("fixtures/torrentio_example.json");
const IMDB: &str = "tt1234567";
const HASH: &str = "0000000000000000000000000000000000000abc";
const POLL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------
// Helpers (the small ones are copies of what tests/worker.rs has)
// ---------------------------------------------------------------------

struct Rig {
    app: TestApp,
    admin: String,
    stub: MockServer,
    worker: Worker<TorrentioSource>,
    /// Kept so the worker never sees a stop.
    _stop: watch::Sender<bool>,
}

async fn rig() -> Rig {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let stub = MockServer::start().await;
    let portal = PortalClient::new(
        &format!("http://{}", app.internal_addr),
        TEST_INTERNAL_TOKEN,
    )
    .unwrap();
    let source = TorrentioSource::new(&stub.uri()).unwrap();
    let (stop, shutdown) = Shutdown::channel();
    let worker =
        Worker::new(portal, source, RankSettings::default(), POLL, shutdown).with_timing(Timing {
            source_retries: [Duration::ZERO; 2],
            report_retries: [Duration::ZERO; 3],
        });
    Rig {
        app,
        admin,
        stub,
        worker,
        _stop: stop,
    }
}

async fn admin_call(
    app: &TestApp,
    cookie: &str,
    method: &str,
    path: &str,
    body: &str,
) -> common::HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("Cookie", cookie));
    app.request(method, path, &headers, Some(body)).await
}

async fn enable(rig: &Rig, body: &str) {
    let response = admin_call(&rig.app, &rig.admin, "PUT", "/api/admin/worker", body).await;
    assert_eq!(response.status, 200, "{}", response.body);
}

/// A queued job for the fixture's film, by a new request; returns the job id.
async fn queue_job(rig: &Rig, tmdb_id: i64, infuse_pro: bool) -> i64 {
    let app = &rig.app;
    let member: Option<i64> = sqlx::query_scalar("SELECT id FROM members WHERE name = 'Alice'")
        .fetch_optional(&app.pool)
        .await
        .unwrap();
    let member = match member {
        Some(id) => id,
        None => {
            common::create_member(&app.pool, &app.config, "Alice")
                .await
                .0
        }
    };
    let now = i64::try_from(app.clock.unix_secs()).unwrap();
    let request: i64 = sqlx::query_scalar(
        "INSERT INTO requests (tmdb_id, media_type, imdb_id, title, year, infuse_pro, status,
                               requested_by, created_at, status_changed_at)
         VALUES (?, 'movie', ?, 'Example Movie', 1994, ?, 'pending', ?, ?, ?) RETURNING id",
    )
    .bind(tmdb_id)
    .bind(IMDB)
    .bind(infuse_pro)
    .bind(member)
    .bind(now)
    .bind(now)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    let response = admin_call(
        app,
        &rig.admin,
        "POST",
        &format!("/api/admin/requests/{request}/fetch"),
        "{}",
    )
    .await;
    assert_eq!(response.status, 201, "{}", response.body);
    let body: Value = serde_json::from_str(&response.body).unwrap();
    body["job"]["id"].as_i64().unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct JobRow {
    state: String,
    reason: Option<String>,
    attempts: i64,
    candidates: Option<String>,
    top_infohash: Option<String>,
    chosen_infohash: Option<String>,
    last_error: Option<String>,
}

async fn job_row(pool: &SqlitePool, id: i64) -> JobRow {
    sqlx::query_as(
        "SELECT state, reason, attempts, candidates, top_infohash, chosen_infohash, last_error
         FROM fetch_jobs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn serve_fixture(stub: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/stream/movie/{IMDB}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(FIXTURE, "application/json"))
        .mount(stub)
        .await;
}

async fn source_requests(stub: &MockServer) -> usize {
    stub.received_requests().await.unwrap().len()
}

// ---------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_free_job_ends_needs_pick_with_candidates_the_portal_accepts() {
    let mut rig = rig().await;
    enable(&rig, r#"{"enabled":true,"mode":"dry_run"}"#).await;
    serve_fixture(&rig.stub).await;
    let job = queue_job(&rig, 1, false).await;

    let cycle = rig.worker.run_once(Instant::now()).await.unwrap();
    assert_eq!(cycle, Cycle::Worked);

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(row.state, "needs_pick");
    assert_eq!(row.reason.as_deref(), Some("dry_run"));
    let candidates: Vec<Value> = serde_json::from_str(row.candidates.as_deref().unwrap()).unwrap();
    assert!(!candidates.is_empty());
    let first = candidates[0].as_object().unwrap();
    for key in ["infohash", "title", "score", "free_badge", "clear_winner"] {
        assert!(first.contains_key(key), "missing {key}");
    }
    assert_eq!(
        row.top_infohash.as_deref(),
        first["infohash"].as_str(),
        "the top hash is the first candidate"
    );
    assert_eq!(source_requests(&rig.stub).await, 1);
}

#[tokio::test]
async fn an_unreachable_source_fails_the_job_and_the_next_cycle_stays_away() {
    let mut rig = rig().await;
    enable(&rig, r#"{"enabled":true,"mode":"dry_run"}"#).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&rig.stub)
        .await;
    let first = queue_job(&rig, 1, false).await;
    let second = queue_job(&rig, 2, false).await;
    let t0 = Instant::now();

    assert_eq!(rig.worker.run_once(t0).await.unwrap(), Cycle::Worked);
    let row = job_row(&rig.app.pool, first).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.reason.as_deref(), Some("source_unreachable"));
    // The first try and the two in-run retries.
    assert_eq!(source_requests(&rig.stub).await, 3);

    // During the backoff nothing is claimed, so no job is burnt.
    let cycle = rig
        .worker
        .run_once(t0 + Duration::from_secs(1))
        .await
        .unwrap();
    assert!(matches!(cycle, Cycle::Wait(left) if left > Duration::from_secs(20)));
    let row = job_row(&rig.app.pool, second).await;
    assert_eq!((row.state.as_str(), row.attempts), ("queued", 0));
    assert_eq!(source_requests(&rig.stub).await, 3);

    // Once it is over the next job is claimed.
    let cycle = rig
        .worker
        .run_once(t0 + Duration::from_secs(31))
        .await
        .unwrap();
    assert_eq!(cycle, Cycle::Worked);
    assert_eq!(job_row(&rig.app.pool, second).await.state, "failed");
}

#[tokio::test]
async fn a_picked_job_is_reported_without_asking_the_source() {
    let mut rig = rig().await;
    enable(&rig, r#"{"enabled":true,"mode":"dry_run"}"#).await;
    let job = queue_job(&rig, 1, false).await;
    sqlx::query("UPDATE fetch_jobs SET picked = 1, chosen_infohash = ? WHERE id = ?")
        .bind(HASH)
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    assert_eq!(
        rig.worker.run_once(Instant::now()).await.unwrap(),
        Cycle::Worked
    );

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(row.state, "needs_pick");
    assert_eq!(row.reason.as_deref(), Some("dry_run"));
    assert_eq!(row.chosen_infohash.as_deref(), Some(HASH));
    assert_eq!(source_requests(&rig.stub).await, 0);
}

#[tokio::test]
async fn a_series_job_fails_without_asking_the_source() {
    let mut rig = rig().await;
    enable(&rig, r#"{"enabled":true,"mode":"dry_run"}"#).await;
    let job = queue_job(&rig, 1, false).await;
    sqlx::query("UPDATE requests SET media_type = 'tv'")
        .execute(&rig.app.pool)
        .await
        .unwrap();

    assert_eq!(
        rig.worker.run_once(Instant::now()).await.unwrap(),
        Cycle::Worked
    );

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.reason, None);
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("series are not supported"))
    );
    assert_eq!(source_requests(&rig.stub).await, 0);
}

#[tokio::test]
async fn a_disabled_worker_and_assisted_mode_claim_nothing() {
    let mut rig = rig().await;
    serve_fixture(&rig.stub).await;
    let job = queue_job(&rig, 1, false).await;

    // The kill switch is off by default.
    let cycle = rig.worker.run_once(Instant::now()).await.unwrap();
    assert_eq!(cycle, Cycle::Wait(POLL));

    enable(&rig, r#"{"enabled":true,"mode":"assisted"}"#).await;
    let cycle = rig.worker.run_once(Instant::now()).await.unwrap();
    assert_eq!(cycle, Cycle::Wait(POLL));

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!((row.state.as_str(), row.attempts), ("queued", 0));
    assert_eq!(source_requests(&rig.stub).await, 0);
}
