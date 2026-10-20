//! The Real-Debrid run against the real portal (public and internal
//! listeners) and a wiremock Real-Debrid. Each case drives `run_once`. All
//! names, hashes, ids and the token are invented; nothing here talks to the
//! real service.

mod common;

use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

use common::{TEST_INTERNAL_TOKEN, TestApp, json_post_headers, login_admin_cookie};
use mediaqueue::worker::{
    config::RdConfig,
    portal::PortalClient,
    rd::{RdRuntime, Secret},
    run::{Cycle, Shutdown, Timing, Worker},
    settings::RankSettings,
    state::StateStore,
    torrentio::TorrentioSource,
};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::watch;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const FIXTURE: &str = include_str!("fixtures/torrentio_example.json");
const IMDB: &str = "tt1234567";
const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RD_ID: &str = "ABCDEFGHIJKLM";
const RD_TOKEN: &str = "test-token-0000";
const POLL: Duration = Duration::from_secs(10);
const GIB: u64 = 1024 * 1024 * 1024;
const MOVIE: &str = "/Example.Movie.1994.1080p.WEB-DL.AAC.mkv";

// ---------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------

struct Rig {
    app: TestApp,
    admin: String,
    rd: MockServer,
    stub: MockServer,
    worker: Worker<TorrentioSource>,
    state: StateStore,
    state_dir: PathBuf,
    _stop: watch::Sender<bool>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.state_dir);
    }
}

fn timing() -> Timing {
    Timing {
        source_retries: [Duration::ZERO; 2],
        report_retries: [Duration::ZERO; 3],
        rd_poll: [Duration::from_millis(5); 7],
        rd_conversion: Duration::from_millis(300),
        rd_picked_wait: Duration::from_millis(200),
        rd_picked_deadline: Duration::from_secs(10),
    }
}

fn state_dir() -> PathBuf {
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("mediaqueue-worker-rd-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

async fn rig() -> Rig {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let rd = MockServer::start().await;
    let stub = MockServer::start().await;
    let state_dir = state_dir();
    let config = RdConfig {
        token: Secret::new(RD_TOKEN),
        base_url: rd.uri(),
        state_dir: state_dir.clone(),
        max_adds_per_day: 5,
        max_uncached_per_day: 2,
        max_bytes: 30 * GIB,
        per_min: 60_000,
        keep_free_slots: 2,
    };
    let runtime = RdRuntime::new(&config).unwrap();
    let state = runtime.state.clone();
    let portal = PortalClient::new(
        &format!("http://{}", app.internal_addr),
        TEST_INTERNAL_TOKEN,
    )
    .unwrap();
    let source = TorrentioSource::new(&stub.uri()).unwrap();
    let (stop, shutdown) = Shutdown::channel();
    let worker = Worker::new(portal, source, RankSettings::default(), POLL, shutdown)
        .with_timing(timing())
        .with_rd(Some(runtime));
    let rig = Rig {
        app,
        admin,
        rd,
        stub,
        worker,
        state,
        state_dir,
        _stop: stop,
    };
    let response = admin_call(
        &rig,
        "PUT",
        "/api/admin/worker",
        r#"{"enabled":true,"mode":"assisted"}"#,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    rig
}

async fn admin_call(rig: &Rig, method: &str, path: &str, body: &str) -> common::HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("Cookie", &rig.admin));
    rig.app.request(method, path, &headers, Some(body)).await
}

/// A queued job for a new pending request; returns the job id.
async fn queue_job(rig: &Rig, tmdb_id: i64) -> i64 {
    let app = &rig.app;
    let member = match sqlx::query_scalar::<_, i64>("SELECT id FROM members WHERE name = 'Alice'")
        .fetch_optional(&app.pool)
        .await
        .unwrap()
    {
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
         VALUES (?, 'movie', ?, 'Example Movie', 1994, 0, 'pending', ?, ?, ?) RETURNING id",
    )
    .bind(tmdb_id)
    .bind(IMDB)
    .bind(member)
    .bind(now)
    .bind(now)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    let response = admin_call(
        rig,
        "POST",
        &format!("/api/admin/requests/{request}/fetch"),
        "{}",
    )
    .await;
    assert_eq!(response.status, 201, "{}", response.body);
    let body: Value = serde_json::from_str(&response.body).unwrap();
    body["job"]["id"].as_i64().unwrap()
}

/// A queued job with the admin's pick on it.
async fn picked_job(rig: &Rig) -> i64 {
    let job = queue_job(rig, 1).await;
    sqlx::query("UPDATE fetch_jobs SET picked = 1, chosen_infohash = ? WHERE id = ?")
        .bind(HASH)
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();
    job
}

#[derive(Debug, sqlx::FromRow)]
struct JobRow {
    state: String,
    reason: Option<String>,
    attempts: i64,
    uncached_adds: i64,
    rd_torrent_id: Option<String>,
    remove_rd_torrent_id: Option<String>,
    chosen_infohash: Option<String>,
    candidates: Option<String>,
    last_error: Option<String>,
}

async fn job_row(pool: &SqlitePool, id: i64) -> JobRow {
    sqlx::query_as(
        "SELECT state, reason, attempts, uncached_adds, rd_torrent_id, remove_rd_torrent_id,
                chosen_infohash, candidates, last_error
         FROM fetch_jobs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn request_status(pool: &SqlitePool, job: i64) -> String {
    sqlx::query_scalar(
        "SELECT r.status FROM requests r JOIN fetch_jobs j ON j.request_id = r.id WHERE j.id = ?",
    )
    .bind(job)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn counters(rig: &Rig) -> Value {
    serde_json::from_slice(&fs::read(rig.state_dir.join("counters.json")).unwrap()).unwrap()
}

async fn run(rig: &mut Rig) -> Cycle {
    rig.worker.run_once(Instant::now()).await.unwrap()
}

// ---------------------------------------------------------------------
// The Real-Debrid stub
// ---------------------------------------------------------------------

fn file(id: u64, path: &str, bytes: u64, selected: bool) -> Value {
    json!({ "id": id, "path": path, "bytes": bytes, "selected": u8::from(selected) })
}

fn info(status: &str, files: Vec<Value>, links: bool) -> Value {
    let selected_bytes: u64 = files
        .iter()
        .filter(|f| f["selected"] == 1)
        .map(|f| f["bytes"].as_u64().unwrap())
        .sum();
    json!({
        "id": RD_ID,
        "filename": "Example.Movie.1994.1080p.WEB-DL.AAC",
        "hash": HASH,
        "bytes": selected_bytes,
        "original_bytes": 5 * GIB,
        "progress": if status == "downloaded" { 100 } else { 40 },
        "status": status,
        "files": files,
        "links": if links { json!(["https://example.invalid/d/1"]) } else { json!([]) },
        "speed": 0,
        "seeders": 12,
    })
}

async fn mount_json(server: &MockServer, verb: &str, route: &str, status: u16, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// Free slots, an empty library, and an add that returns `RD_ID`.
async fn mount_roomy(rd: &MockServer) {
    mount_json(
        rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 1, "limit": 25}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(204))
        .mount(rd)
        .await;
    mount_json(
        rd,
        "POST",
        "/torrents/addMagnet",
        201,
        json!({"id": RD_ID, "uri": "https://example.invalid/t/1"}),
    )
    .await;
}

/// The torrent is cached: the file list first, then done once a file is
/// selected.
async fn mount_cached_torrent(rd: &MockServer) {
    let waiting = info(
        "waiting_files_selection",
        vec![
            file(1, MOVIE, 4 * GIB, false),
            file(2, "/info.nfo", 1_200, false),
        ],
        false,
    );
    Mock::given(method("GET"))
        .and(path(format!("/torrents/info/{RD_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(waiting))
        .up_to_n_times(1)
        .mount(rd)
        .await;
    let done = info(
        "downloaded",
        vec![
            file(1, MOVIE, 4 * GIB, true),
            file(2, "/info.nfo", 1_200, false),
        ],
        true,
    );
    mount_json(rd, "GET", &format!("/torrents/info/{RD_ID}"), 200, done).await;
    Mock::given(method("POST"))
        .and(path(format!("/torrents/selectFiles/{RD_ID}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(rd)
        .await;
}

async fn mount_delete(rd: &MockServer, id: &str) {
    Mock::given(method("DELETE"))
        .and(path(format!("/torrents/delete/{id}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(rd)
        .await;
}

async fn rd_calls(rig: &Rig, verb: &str, route_prefix: &str) -> usize {
    rig.rd
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == verb && r.url.path().starts_with(route_prefix))
        .count()
}

async fn serve_fixture(stub: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/stream/movie/{IMDB}.json")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(FIXTURE, "application/json"))
        .mount(stub)
        .await;
}

// ---------------------------------------------------------------------
// The picked run
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_cached_torrent_ends_done_with_the_request_added_and_the_torrent_owned() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    mount_cached_torrent(&rig.rd).await;
    let job = picked_job(&rig).await;

    assert_eq!(run(&mut rig).await, Cycle::Worked);

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.rd_torrent_id.as_deref(), row.reason),
        ("done", Some(RD_ID), None)
    );
    assert_eq!(row.chosen_infohash.as_deref(), Some(HASH));
    assert_eq!(request_status(&rig.app.pool, job).await, "added");

    // The file was chosen by the worker, one file only.
    let selects: Vec<_> = rig
        .rd
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().starts_with("/torrents/selectFiles/"))
        .collect();
    assert_eq!(selects.len(), 1);
    assert_eq!(String::from_utf8_lossy(&selects[0].body), "files=1");

    // One add counted, the torrent recorded as ours, nothing deleted.
    let saved = counters(&rig);
    assert_eq!(saved["adds"], 1);
    assert_eq!(saved["owned"][0]["id"], RD_ID);
    assert_eq!(saved["owned"][0]["hash"], HASH);
    assert!(rig.state.owns(RD_ID));
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
}

#[tokio::test]
async fn the_token_is_nowhere_in_the_portal_or_the_state_file() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    mount_cached_torrent(&rig.rd).await;
    let job = picked_job(&rig).await;
    run(&mut rig).await;

    let rows: Vec<(
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT candidates, last_error, reason, rd_torrent_id FROM fetch_jobs WHERE id = ?",
    )
    .bind(job)
    .fetch_all(&rig.app.pool)
    .await
    .unwrap();
    assert!(!format!("{rows:?}").contains("test-token"));
    let events: Vec<(Option<String>,)> = sqlx::query_as("SELECT actor FROM request_events")
        .fetch_all(&rig.app.pool)
        .await
        .unwrap();
    assert!(!format!("{events:?}").contains("test-token"));
    let saved = fs::read_to_string(rig.state_dir.join("counters.json")).unwrap();
    assert!(!saved.contains("test-token"));
    // And it did go to Real-Debrid, as a header.
    let requests = rig.rd.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    for request in requests {
        assert_eq!(
            request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {RD_TOKEN}")
        );
    }
}

#[tokio::test]
async fn a_refused_add_ends_picked_failed_and_nothing_is_owned() {
    let mut rig = rig().await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 0, "limit": 25}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&rig.rd)
        .await;
    mount_json(
        &rig.rd,
        "POST",
        "/torrents/addMagnet",
        403,
        json!({"error": "infringing_file", "error_code": 35}),
    )
    .await;
    let job = picked_job(&rig).await;

    assert_eq!(run(&mut rig).await, Cycle::Worked);

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("picked_failed"))
    );
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("error 35"))
    );
    assert_eq!(row.rd_torrent_id, None);
    assert_eq!(row.chosen_infohash.as_deref(), Some(HASH), "the pick stays");
    let saved = counters(&rig);
    assert_eq!(saved["owned"], json!([]));
    // The add was counted before it was tried; a refusal is not a breaker trip.
    assert_eq!(saved["adds"], 1);
    assert_eq!(saved["breaker_until"], 0);
}

#[tokio::test]
async fn a_torrent_with_an_executable_is_deleted_and_the_pick_fails() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    let files = vec![
        file(1, MOVIE, 4 * GIB, false),
        file(2, "/Example.Movie.exe", 90_000, false),
    ];
    mount_json(
        &rig.rd,
        "GET",
        &format!("/torrents/info/{RD_ID}"),
        200,
        info("waiting_files_selection", files, false),
    )
    .await;
    mount_delete(&rig.rd, RD_ID).await;
    let job = picked_job(&rig).await;

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("picked_failed"))
    );
    assert!(
        row.last_error
            .as_deref()
            .is_some_and(|e| e.contains("unsafe"))
    );
    assert_eq!(rd_calls(&rig, "POST", "/torrents/selectFiles/").await, 0);
    assert_eq!(
        rd_calls(&rig, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
    assert!(!rig.state.owns(RD_ID));
    assert_eq!(request_status(&rig.app.pool, job).await, "pending");
}

#[tokio::test]
async fn a_torrent_that_stalls_downloading_is_deleted_and_counted_as_uncached() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    let waiting = info(
        "waiting_files_selection",
        vec![file(1, MOVIE, 4 * GIB, false)],
        false,
    );
    Mock::given(method("GET"))
        .and(path(format!("/torrents/info/{RD_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(waiting))
        .up_to_n_times(1)
        .mount(&rig.rd)
        .await;
    mount_json(
        &rig.rd,
        "GET",
        &format!("/torrents/info/{RD_ID}"),
        200,
        info("downloading", vec![file(1, MOVIE, 4 * GIB, true)], false),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("/torrents/selectFiles/{RD_ID}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&rig.rd)
        .await;
    mount_delete(&rig.rd, RD_ID).await;
    let job = picked_job(&rig).await;

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("picked_failed"))
    );
    assert_eq!(row.uncached_adds, 1);
    assert_eq!(counters(&rig)["uncached"], 1);
    assert_eq!(
        rd_calls(&rig, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
    assert!(!rig.state.owns(RD_ID));
}

#[tokio::test]
async fn a_hash_already_downloaded_in_the_library_is_already_had_with_no_add() {
    let mut rig = rig().await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 1, "limit": 25}),
    )
    .await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents",
        200,
        json!([{ "id": "EXISTING1", "hash": HASH.to_uppercase(), "status": "downloaded", "filename": "x" }]),
    )
    .await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/info/EXISTING1",
        200,
        json!({
            "id": "EXISTING1", "hash": HASH, "status": "downloaded", "progress": 100,
            "bytes": 4 * GIB, "files": [file(1, MOVIE, 4 * GIB, true)],
            "links": ["https://example.invalid/d/1"],
        }),
    )
    .await;
    let job = picked_job(&rig).await;

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref(), row.rd_torrent_id),
        ("done", Some("already_had"), None)
    );
    assert_eq!(request_status(&rig.app.pool, job).await, "already_had");
    assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 0);
    assert!(!rig.state.owns("EXISTING1"));
    assert!(
        !rig.state_dir.join("counters.json").exists(),
        "no add was counted"
    );
}

#[tokio::test]
async fn a_library_hit_that_is_not_downloaded_is_left_alone() {
    let mut rig = rig().await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 1, "limit": 25}),
    )
    .await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents",
        200,
        json!([{ "id": "EXISTING1", "hash": HASH, "status": "downloading" }]),
    )
    .await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/info/EXISTING1",
        200,
        info("downloading", vec![file(1, MOVIE, 4 * GIB, true)], false),
    )
    .await;
    let job = picked_job(&rig).await;

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("rd_error"))
    );
    assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 0);
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
}

#[tokio::test]
async fn no_free_slot_ends_needs_pick_without_adding() {
    let mut rig = rig().await;
    // 23 of 25 active with two slots kept free.
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 23, "limit": 25}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&rig.rd)
        .await;
    let job = picked_job(&rig).await;

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("rd_slots_full"))
    );
    assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 0);
    assert!(!rig.state_dir.join("counters.json").exists());
}

#[tokio::test]
async fn reaching_the_daily_cap_claims_nothing() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    for _ in 0..5 {
        rig.state.record_add().unwrap();
    }
    let job = picked_job(&rig).await;

    let cycle = run(&mut rig).await;

    assert_eq!(cycle, Cycle::Wait(POLL));
    let row = job_row(&rig.app.pool, job).await;
    assert_eq!((row.state.as_str(), row.attempts), ("queued", 0));
    assert!(rig.rd.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_open_breaker_claims_nothing() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    rig.state
        .trip(mediaqueue::worker::state::Trip::RateLimit, rig.state.now())
        .unwrap();
    let job = picked_job(&rig).await;

    assert_eq!(run(&mut rig).await, Cycle::Wait(POLL));
    assert_eq!(job_row(&rig.app.pool, job).await.attempts, 0);
    assert!(rig.rd.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_owned_torrent_from_an_earlier_run_is_resumed_not_added_again() {
    let mut rig = rig().await;
    mount_json(
        &rig.rd,
        "GET",
        &format!("/torrents/info/{RD_ID}"),
        200,
        info("downloaded", vec![file(1, MOVIE, 4 * GIB, true)], true),
    )
    .await;
    rig.state.own(RD_ID, HASH).unwrap();
    let job = picked_job(&rig).await;
    sqlx::query("UPDATE fetch_jobs SET rd_torrent_id = ? WHERE id = ?")
        .bind(RD_ID)
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.rd_torrent_id.as_deref()),
        ("done", Some(RD_ID))
    );
    assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 0);
    assert_eq!(rd_calls(&rig, "GET", "/torrents/activeCount").await, 0);
}

#[tokio::test]
async fn a_torrent_id_from_the_portal_that_is_not_owned_is_never_deleted() {
    let mut rig = rig().await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 0, "limit": 25}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&rig.rd)
        .await;
    mount_json(
        &rig.rd,
        "POST",
        "/torrents/addMagnet",
        403,
        json!({"error_code": 35}),
    )
    .await;
    mount_delete(&rig.rd, "FOREIGN1").await;
    let job = picked_job(&rig).await;
    sqlx::query("UPDATE fetch_jobs SET rd_torrent_id = 'FOREIGN1' WHERE id = ?")
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    run(&mut rig).await;

    assert_eq!(
        job_row(&rig.app.pool, job).await.reason.as_deref(),
        Some("picked_failed")
    );
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
    assert_eq!(rd_calls(&rig, "GET", "/torrents/info/").await, 0);
}

// ---------------------------------------------------------------------
// Unpicked jobs and removals
// ---------------------------------------------------------------------

#[tokio::test]
async fn an_unpicked_job_in_assisted_mode_waits_for_a_pick_with_the_shortlist() {
    let mut rig = rig().await;
    serve_fixture(&rig.stub).await;
    let job = queue_job(&rig, 1).await;

    assert_eq!(run(&mut rig).await, Cycle::Worked);

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("no_clear_choice"))
    );
    let candidates: Vec<Value> = serde_json::from_str(row.candidates.as_deref().unwrap()).unwrap();
    assert!(!candidates.is_empty());
    assert!(rig.rd.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn auto_mode_behaves_like_assisted_for_now() {
    let mut rig = rig().await;
    admin_call(&rig, "PUT", "/api/admin/worker", r#"{"mode":"auto"}"#).await;
    serve_fixture(&rig.stub).await;
    let job = queue_job(&rig, 1).await;

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(row.reason.as_deref(), Some("no_clear_choice"));
}

#[tokio::test]
async fn an_owned_removal_is_deleted_and_cleared() {
    let mut rig = rig().await;
    serve_fixture(&rig.stub).await;
    mount_delete(&rig.rd, "OWNED1").await;
    rig.state.own("OWNED1", HASH).unwrap();
    let job = queue_job(&rig, 1).await;
    sqlx::query("UPDATE fetch_jobs SET remove_rd_torrent_id = 'OWNED1' WHERE id = ?")
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    run(&mut rig).await;

    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/OWNED1").await, 1);
    assert!(!rig.state.owns("OWNED1"));
    assert_eq!(job_row(&rig.app.pool, job).await.remove_rd_torrent_id, None);
}

#[tokio::test]
async fn a_removal_of_a_torrent_not_owned_is_cleared_without_a_delete() {
    let mut rig = rig().await;
    serve_fixture(&rig.stub).await;
    mount_delete(&rig.rd, "FOREIGN1").await;
    let job = queue_job(&rig, 1).await;
    sqlx::query("UPDATE fetch_jobs SET remove_rd_torrent_id = 'FOREIGN1' WHERE id = ?")
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    run(&mut rig).await;

    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
    assert_eq!(job_row(&rig.app.pool, job).await.remove_rd_torrent_id, None);
}

#[tokio::test]
async fn a_removal_that_cannot_be_deleted_now_is_queued_and_still_cleared() {
    let mut rig = rig().await;
    serve_fixture(&rig.stub).await;
    Mock::given(method("DELETE"))
        .and(path("/torrents/delete/OWNED1"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&rig.rd)
        .await;
    rig.state.own("OWNED1", HASH).unwrap();
    let job = queue_job(&rig, 1).await;
    sqlx::query("UPDATE fetch_jobs SET remove_rd_torrent_id = 'OWNED1' WHERE id = ?")
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    run(&mut rig).await;

    assert!(rig.state.owns("OWNED1"), "still ours until it is gone");
    assert_eq!(counters(&rig)["pending_delete"], json!(["OWNED1"]));
    assert_eq!(job_row(&rig.app.pool, job).await.remove_rd_torrent_id, None);
}
