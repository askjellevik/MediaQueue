//! The Real-Debrid run against the real portal (public and internal
//! listeners) and a wiremock Real-Debrid. Each case drives `run_once`. All
//! names, hashes, ids and the token are invented; nothing here talks to the
//! real service.

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use common::{TEST_INTERNAL_TOKEN, TestApp, json_post_headers, login_admin_cookie};
use mediaqueue::worker::{
    config::RdConfig,
    portal::PortalClient,
    rd::{RdRuntime, Secret},
    run::{Cycle, Shutdown, Timing, Worker},
    settings::RankSettings,
    state::{Clock, StateStore},
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
    worker: Option<Worker<TorrentioSource>>,
    state: StateStore,
    state_dir: PathBuf,
    _stop: watch::Sender<bool>,
    _dir: TempDir,
}

/// A directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A clock the test moves.
struct TestClock(AtomicU64);

impl TestClock {
    fn new() -> Arc<Self> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Arc::new(Self(AtomicU64::new(now)))
    }
    fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_unix(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
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

fn state_dir() -> TempDir {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("mediaqueue-worker-rd-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

struct Built {
    worker: Worker<TorrentioSource>,
    state: StateStore,
    stop: watch::Sender<bool>,
}

/// A worker against `portal_url`, the Real-Debrid stub and the state in
/// `dir`. `clock` replaces the system clock for the counters.
fn build_worker(
    portal_url: &str,
    rd: &MockServer,
    stub: &MockServer,
    dir: &Path,
    clock: Option<Arc<dyn Clock>>,
    timing: Timing,
) -> Built {
    let config = RdConfig {
        token: Secret::new(RD_TOKEN),
        base_url: rd.uri(),
        state_dir: dir.to_path_buf(),
        max_adds_per_day: 50,
        max_uncached_per_day: 2,
        max_bytes: 30 * GIB,
        per_min: 60_000,
        keep_free_slots: 2,
    };
    let runtime = match clock {
        Some(clock) => RdRuntime::with_clock(&config, clock),
        None => RdRuntime::new(&config),
    }
    .unwrap();
    let state = runtime.state.clone();
    let portal = PortalClient::new(portal_url, TEST_INTERNAL_TOKEN).unwrap();
    let source = TorrentioSource::new(&stub.uri()).unwrap();
    let (stop, shutdown) = Shutdown::channel();
    let worker = Worker::new(portal, source, RankSettings::default(), POLL, shutdown)
        .with_timing(timing)
        .with_rd(Some(runtime));
    Built {
        worker,
        state,
        stop,
    }
}

async fn rig() -> Rig {
    rig_with(None, timing()).await
}

async fn rig_with(clock: Option<Arc<dyn Clock>>, timing: Timing) -> Rig {
    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let rd = MockServer::start().await;
    let stub = MockServer::start().await;
    let dir = state_dir();
    let built = build_worker(
        &format!("http://{}", app.internal_addr),
        &rd,
        &stub,
        &dir.0,
        clock,
        timing,
    );
    let rig = Rig {
        app,
        admin,
        rd,
        stub,
        worker: Some(built.worker),
        state: built.state,
        state_dir: dir.0.clone(),
        _stop: built.stop,
        _dir: dir,
    };
    let response = admin_call(
        &rig.app,
        &rig.admin,
        "PUT",
        "/api/admin/worker",
        r#"{"enabled":true,"mode":"assisted"}"#,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    rig
}

async fn admin_call(
    app: &TestApp,
    admin: &str,
    method: &str,
    path: &str,
    body: &str,
) -> common::HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("Cookie", admin));
    app.request(method, path, &headers, Some(body)).await
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

/// The admin marked `hash` as unusable for the job's request.
async fn block_hash(rig: &Rig, job: i64, hash: &str) {
    sqlx::query(
        "INSERT INTO blocked_hashes (request_id, infohash, created_at)
         SELECT request_id, ?, 1 FROM fetch_jobs WHERE id = ?",
    )
    .bind(hash)
    .bind(job)
    .execute(&rig.app.pool)
    .await
    .unwrap();
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
    rig.worker
        .as_mut()
        .unwrap()
        .run_once(Instant::now())
        .await
        .unwrap()
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

/// The file list, then a download that never finishes.
async fn mount_stalled_torrent(rd: &MockServer) {
    let waiting = info(
        "waiting_files_selection",
        vec![file(1, MOVIE, 4 * GIB, false)],
        false,
    );
    Mock::given(method("GET"))
        .and(path(format!("/torrents/info/{RD_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(waiting))
        .up_to_n_times(1)
        .mount(rd)
        .await;
    mount_json(
        rd,
        "GET",
        &format!("/torrents/info/{RD_ID}"),
        200,
        info("downloading", vec![file(1, MOVIE, 4 * GIB, true)], false),
    )
    .await;
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

    // The pick is counted apart from the daily cap, the torrent recorded as
    // ours, nothing deleted.
    let saved = counters(&rig);
    assert_eq!(
        (saved["adds"].as_u64(), saved["human_adds"].as_u64()),
        (Some(0), Some(1))
    );
    assert_eq!(saved["owned"][0]["id"], RD_ID);
    assert_eq!(saved["owned"][0]["hash"], HASH);
    assert!(rig.state.owns(RD_ID));
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
}

/// Collects what the current thread logs while it is alive.
#[derive(Clone, Default)]
struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// What the current thread logs while the guard lives (a test runs on one
/// thread, and so do the portal and the stubs it talks to).
fn capture_logs(level: tracing::Level) -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let logs = LogCapture::default();
    let writer = logs.clone();
    let guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_writer(move || writer.clone())
            .finish(),
    );
    (logs, guard)
}

/// Text that must never reach a log: the token and anything Real-Debrid
/// sends in a body (names, links, error texts).
const SECRETS: [&str; 6] = [
    RD_TOKEN,
    "test-token",
    "example.invalid",
    "Example.Movie.1994.1080p.WEB-DL.AAC",
    "info.nfo",
    "infringing",
];

/// A finished run and a refused one, start to end, with the token echoed in
/// an error body. Checks the portal, the state file and the logs.
async fn two_full_runs_and_their_logs(level: tracing::Level) {
    let (logs, _guard) = capture_logs(level);

    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    mount_cached_torrent(&rig.rd).await;
    let job = picked_job(&rig).await;
    run(&mut rig).await;
    assert_eq!(job_row(&rig.app.pool, job).await.state, "done");
    // The token went to Real-Debrid, as a header and nowhere else.
    let requests = rig.rd.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    for request in requests {
        let header = request.headers.get("authorization").unwrap();
        assert_eq!(header.to_str().unwrap(), format!("Bearer {RD_TOKEN}"));
    }
    let row = job_row(&rig.app.pool, job).await;
    assert!(!format!("{row:?}").contains("test-token"));
    let saved = fs::read_to_string(rig.state_dir.join("counters.json")).unwrap();
    assert!(!saved.contains("test-token"));

    let mut refused = self::rig().await;
    mount_json(
        &refused.rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 0, "limit": 25}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&refused.rd)
        .await;
    mount_json(
        &refused.rd,
        "POST",
        "/torrents/addMagnet",
        403,
        json!({
            "error": format!("Example.Movie.1994.1080p.WEB-DL.AAC is infringing, {RD_TOKEN}"),
            "error_code": 35,
        }),
    )
    .await;
    let job = picked_job(&refused).await;
    run(&mut refused).await;
    let row = job_row(&refused.app.pool, job).await;
    assert_eq!(row.reason.as_deref(), Some("picked_failed"));
    assert!(!format!("{row:?}").contains("test-token"));
    assert!(!format!("{row:?}").contains("infringing"));

    let text = logs.text();
    assert!(
        text.contains("downloaded and verified"),
        "the run was logged"
    );
    for secret in SECRETS {
        assert!(
            !text.contains(secret),
            "{secret} reached the log at {level}"
        );
    }
}

#[tokio::test]
async fn neither_the_token_nor_a_response_body_reaches_the_logs_of_a_full_run_at_debug() {
    two_full_runs_and_their_logs(tracing::Level::DEBUG).await;
}

#[tokio::test]
async fn neither_the_token_nor_a_response_body_reaches_the_logs_of_a_full_run_at_trace() {
    two_full_runs_and_their_logs(tracing::Level::TRACE).await;
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
    assert_eq!(saved["human_adds"], 1);
    assert_eq!(saved["breaker_until"], 0);
}

#[tokio::test]
async fn a_malformed_hash_fails_before_any_add_is_counted() {
    for bad in [
        "z".repeat(40),
        format!("{}g", "a".repeat(39)),
        "a".repeat(39),
    ] {
        let mut rig = rig().await;
        mount_roomy(&rig.rd).await;
        let job = picked_job(&rig).await;
        sqlx::query("UPDATE fetch_jobs SET chosen_infohash = ? WHERE id = ?")
            .bind(&bad)
            .bind(job)
            .execute(&rig.app.pool)
            .await
            .unwrap();

        run(&mut rig).await;

        let row = job_row(&rig.app.pool, job).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref()),
            ("needs_pick", Some("picked_failed")),
            "{bad}"
        );
        assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 0);
        assert!(
            !rig.state_dir.join("counters.json").exists(),
            "no add was counted for {bad}"
        );
    }
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
async fn a_torrent_that_stalls_downloading_is_deleted_and_reported_as_uncached() {
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
    // A human's pick is not counted against the uncached cap.
    assert_eq!(counters(&rig)["uncached"], 0);
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
async fn a_library_hit_whose_file_is_a_sample_or_over_the_cap_is_not_already_had() {
    for (path_in_torrent, bytes) in [
        ("/Sample/example.movie.1994.mkv", 4 * GIB),
        ("/Example.Movie.1994-sample.mkv", 4 * GIB),
        (MOVIE, 31 * GIB),
    ] {
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
            json!([{ "id": "EXISTING1", "hash": HASH, "status": "downloaded" }]),
        )
        .await;
        mount_json(
            &rig.rd,
            "GET",
            "/torrents/info/EXISTING1",
            200,
            json!({
                "id": "EXISTING1", "hash": HASH, "status": "downloaded", "progress": 100,
                "bytes": bytes, "files": [file(1, path_in_torrent, bytes, true)],
                "links": ["https://example.invalid/d/1"],
            }),
        )
        .await;
        let job = picked_job(&rig).await;

        run(&mut rig).await;

        let row = job_row(&rig.app.pool, job).await;
        assert_eq!(
            (row.state.as_str(), row.reason.as_deref()),
            ("needs_pick", Some("rd_error")),
            "{path_in_torrent}"
        );
        assert_eq!(request_status(&rig.app.pool, job).await, "pending");
        assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 0);
    }
}

#[tokio::test]
async fn an_add_that_returns_an_id_already_in_the_library_is_left_alone() {
    let (logs, _guard) = capture_logs(tracing::Level::INFO);
    let mut rig = rig().await;
    mount_json(
        &rig.rd,
        "GET",
        "/torrents/activeCount",
        200,
        json!({"nb": 1, "limit": 25}),
    )
    .await;
    // Someone else's torrent for another hash is in the library, and
    // addMagnet answers with its id.
    mount_json(
        &rig.rd,
        "GET",
        "/torrents",
        200,
        json!([{ "id": RD_ID, "hash": "b".repeat(40), "status": "downloaded" }]),
    )
    .await;
    mount_json(
        &rig.rd,
        "POST",
        "/torrents/addMagnet",
        201,
        json!({"id": RD_ID, "uri": "https://example.invalid/t/1"}),
    )
    .await;
    mount_delete(&rig.rd, RD_ID).await;
    let job = picked_job(&rig).await;

    run(&mut rig).await;

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref(), row.rd_torrent_id),
        ("needs_pick", Some("picked_failed"), None)
    );
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
    assert_eq!(rd_calls(&rig, "GET", "/torrents/info/").await, 0);
    assert_eq!(rd_calls(&rig, "POST", "/torrents/selectFiles/").await, 0);
    assert!(!rig.state.owns(RD_ID));
    assert_eq!(counters(&rig)["owned"], json!([]));

    let text = logs.text();
    let lines: Vec<_> = text
        .lines()
        .filter(|l| l.contains("already in the library"))
        .collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(!lines[0].contains(RD_TOKEN) && !lines[0].contains("example.invalid"));
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
async fn a_human_pick_is_neither_counted_nor_blocked_at_the_daily_cap() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    mount_cached_torrent(&rig.rd).await;
    while rig.state.adds_left() > 0 {
        rig.state
            .record_add(mediaqueue::worker::state::AddKind::Auto)
            .unwrap();
    }
    assert!(rig.state.auto_caps_reached());
    let job = picked_job(&rig).await;

    assert_eq!(run(&mut rig).await, Cycle::Worked);

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!((row.state.as_str(), row.reason), ("done", None));
    let saved = counters(&rig);
    assert_eq!(
        (saved["adds"].as_u64(), saved["human_adds"].as_u64()),
        (Some(50), Some(1))
    );
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
    admin_call(
        &rig.app,
        &rig.admin,
        "PUT",
        "/api/admin/worker",
        r#"{"mode":"auto"}"#,
    )
    .await;
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
    block_hash(&rig, job, HASH).await;
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
    block_hash(&rig, job, HASH).await;
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

#[tokio::test]
async fn a_removal_naming_a_torrent_owned_for_another_title_deletes_nothing() {
    let mut rig = rig().await;
    serve_fixture(&rig.stub).await;
    mount_delete(&rig.rd, "OWNED1").await;
    // Ours, but for a hash this job knows nothing about.
    rig.state.own("OWNED1", &"c".repeat(40)).unwrap();
    let job = queue_job(&rig, 1).await;
    block_hash(&rig, job, HASH).await;
    sqlx::query("UPDATE fetch_jobs SET remove_rd_torrent_id = 'OWNED1' WHERE id = ?")
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    run(&mut rig).await;

    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
    assert!(
        rig.state.owns("OWNED1"),
        "still ours, and still the other title's"
    );
    assert_eq!(job_row(&rig.app.pool, job).await.remove_rd_torrent_id, None);
}

#[tokio::test]
async fn a_removal_of_the_torrent_owned_for_the_chosen_hash_is_deleted() {
    let mut rig = rig().await;
    serve_fixture(&rig.stub).await;
    mount_delete(&rig.rd, "OWNED1").await;
    rig.state.own("OWNED1", HASH).unwrap();
    let job = queue_job(&rig, 1).await;
    sqlx::query(
        "UPDATE fetch_jobs SET chosen_infohash = ?, remove_rd_torrent_id = 'OWNED1' WHERE id = ?",
    )
    .bind(HASH)
    .bind(job)
    .execute(&rig.app.pool)
    .await
    .unwrap();

    run(&mut rig).await;

    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/OWNED1").await, 1);
    assert!(!rig.state.owns("OWNED1"));
}

#[tokio::test]
async fn an_owned_torrent_for_another_hash_is_not_resumed() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    mount_cached_torrent(&rig.rd).await;
    // The portal names RD_ID for the job, and the worker owns RD_ID, but it
    // added it for some other release.
    rig.state.own(RD_ID, &"c".repeat(40)).unwrap();
    let job = picked_job(&rig).await;
    sqlx::query("UPDATE fetch_jobs SET rd_torrent_id = ? WHERE id = ?")
        .bind(RD_ID)
        .bind(job)
        .execute(&rig.app.pool)
        .await
        .unwrap();

    run(&mut rig).await;

    assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 1);
    assert_eq!(counters(&rig)["owned"][0]["hash"], HASH);
}

// ---------------------------------------------------------------------
// Runs that are stopped from outside
// ---------------------------------------------------------------------

/// Waits that last until the test interferes.
fn slow_timing() -> Timing {
    Timing {
        rd_poll: [Duration::from_millis(20); 7],
        rd_conversion: Duration::from_secs(20),
        rd_picked_wait: Duration::from_secs(20),
        rd_picked_deadline: Duration::from_secs(60),
        ..timing()
    }
}

/// Polls until `check` holds, for at most five seconds.
async fn wait_until<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..500 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the condition never held");
}

#[tokio::test]
async fn cancelling_during_the_wait_drops_the_run_and_deletes_the_torrent_it_owns() {
    let mut rig = rig_with(None, slow_timing()).await;
    mount_roomy(&rig.rd).await;
    mount_stalled_torrent(&rig.rd).await;
    mount_delete(&rig.rd, RD_ID).await;
    let job = picked_job(&rig).await;

    let (cycle, ()) = tokio::join!(
        rig.worker.as_mut().unwrap().run_once(Instant::now()),
        async {
            wait_until(|| async { job_row(&rig.app.pool, job).await.rd_torrent_id.is_some() })
                .await;
            let response = admin_call(
                &rig.app,
                &rig.admin,
                "POST",
                &format!("/api/admin/jobs/{job}/cancel"),
                "{}",
            )
            .await;
            assert_eq!(response.status, 200, "{}", response.body);
        }
    );

    assert_eq!(cycle.unwrap(), Cycle::Worked);
    // The portal moved the torrent to the removal column, so the stale
    // answer names nothing to keep; the run deleted what it owns and
    // reported nothing over the cancellation.
    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.reason.as_deref(),
            row.rd_torrent_id.as_deref(),
            row.remove_rd_torrent_id.as_deref()
        ),
        ("failed", Some("cancelled"), None, Some(RD_ID))
    );
    assert_eq!(
        rd_calls(&rig, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
    assert!(!rig.state.owns(RD_ID));
}

#[tokio::test]
async fn retrying_during_the_wait_drops_the_run_and_keeps_the_torrent() {
    let mut rig = rig_with(None, slow_timing()).await;
    mount_roomy(&rig.rd).await;
    mount_stalled_torrent(&rig.rd).await;
    mount_delete(&rig.rd, RD_ID).await;
    let job = picked_job(&rig).await;

    let (cycle, ()) = tokio::join!(
        rig.worker.as_mut().unwrap().run_once(Instant::now()),
        async {
            wait_until(|| async { job_row(&rig.app.pool, job).await.rd_torrent_id.is_some() })
                .await;
            let response = admin_call(
                &rig.app,
                &rig.admin,
                "POST",
                &format!("/api/admin/jobs/{job}/retry"),
                "{}",
            )
            .await;
            assert_eq!(response.status, 200, "{}", response.body);
        }
    );

    assert_eq!(cycle.unwrap(), Cycle::Worked);
    // The portal still names the torrent, so the next run resumes it.
    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.rd_torrent_id.as_deref(),
            row.remove_rd_torrent_id
        ),
        ("queued", Some(RD_ID), None)
    );
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
    assert!(rig.state.owns(RD_ID));
}

#[tokio::test]
async fn a_run_whose_claim_is_gone_deletes_a_torrent_the_portal_no_longer_names() {
    let mut rig = rig_with(None, slow_timing()).await;
    serve_fixture(&rig.stub).await;
    mount_roomy(&rig.rd).await;
    mount_stalled_torrent(&rig.rd).await;
    mount_delete(&rig.rd, RD_ID).await;
    let job = picked_job(&rig).await;

    let (cycle, ()) = tokio::join!(
        rig.worker.as_mut().unwrap().run_once(Instant::now()),
        async {
            wait_until(|| async { job_row(&rig.app.pool, job).await.rd_torrent_id.is_some() })
                .await;
            // "Didn't play" moves the stored torrent to the removal column,
            // so the 409 the worker gets names nothing.
            let response = admin_call(
                &rig.app,
                &rig.admin,
                "POST",
                &format!("/api/admin/jobs/{job}/not-playing"),
                "{}",
            )
            .await;
            assert_eq!(response.status, 200, "{}", response.body);
        }
    );

    assert_eq!(cycle.unwrap(), Cycle::Worked);
    assert_eq!(
        rd_calls(&rig, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
    assert!(!rig.state.owns(RD_ID));
    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.rd_torrent_id,
            row.remove_rd_torrent_id.as_deref()
        ),
        ("queued", None, Some(RD_ID))
    );

    // The next claim finds a removal for a torrent that is already gone and
    // clears it without another delete.
    assert_eq!(run(&mut rig).await, Cycle::Worked);
    assert_eq!(job_row(&rig.app.pool, job).await.remove_rd_torrent_id, None);
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 1);
}

/// The file list, then Real-Debrid asks us to slow down.
async fn mount_rate_limit_on_the_poll(rd: &MockServer) {
    let waiting = info(
        "waiting_files_selection",
        vec![file(1, MOVIE, 4 * GIB, false)],
        false,
    );
    Mock::given(method("GET"))
        .and(path(format!("/torrents/info/{RD_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(waiting))
        .up_to_n_times(1)
        .mount(rd)
        .await;
    mount_json(
        rd,
        "GET",
        &format!("/torrents/info/{RD_ID}"),
        429,
        json!({"error": "too_many_requests", "error_code": 34}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("/torrents/selectFiles/{RD_ID}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(rd)
        .await;
}

#[tokio::test]
async fn a_rate_limit_on_the_poll_opens_the_breaker_and_queues_the_deletion() {
    let mut rig = rig().await;
    mount_roomy(&rig.rd).await;
    mount_rate_limit_on_the_poll(&rig.rd).await;
    mount_delete(&rig.rd, RD_ID).await;
    let first = picked_job(&rig).await;
    let second = queue_job(&rig, 2).await;

    assert_eq!(run(&mut rig).await, Cycle::Worked);

    let row = job_row(&rig.app.pool, first).await;
    assert_eq!(
        (row.state.as_str(), row.reason.as_deref()),
        ("needs_pick", Some("rate_limited"))
    );
    let saved = counters(&rig);
    assert!(saved["breaker_until"].as_u64().unwrap() > 0);
    assert_eq!(saved["pending_delete"], json!([RD_ID]));
    assert!(rig.state.owns(RD_ID), "still ours until it is gone");
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);

    // While the breaker is open nothing is claimed and nothing is called.
    let calls = rig.rd.received_requests().await.unwrap().len();
    assert_eq!(run(&mut rig).await, Cycle::Wait(POLL));
    assert_eq!(rig.rd.received_requests().await.unwrap().len(), calls);
    assert_eq!(job_row(&rig.app.pool, second).await.attempts, 0);
}

#[tokio::test]
async fn a_restarted_worker_deletes_the_queued_torrent_once_the_breaker_has_passed() {
    let clock = TestClock::new();
    let mut rig = rig_with(Some(clock.clone()), timing()).await;
    mount_roomy(&rig.rd).await;
    mount_rate_limit_on_the_poll(&rig.rd).await;
    mount_delete(&rig.rd, RD_ID).await;
    let first = picked_job(&rig).await;
    run(&mut rig).await;
    assert_eq!(counters(&rig)["pending_delete"], json!([RD_ID]));
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);
    assert_eq!(
        job_row(&rig.app.pool, first).await.reason.as_deref(),
        Some("rate_limited")
    );

    // A new process over the same state, while the breaker is still open.
    let mut again = build_worker(
        &format!("http://{}", rig.app.internal_addr),
        &rig.rd,
        &rig.stub,
        &rig.state_dir,
        Some(clock.clone()),
        timing(),
    );
    again.worker.run_once(Instant::now()).await.unwrap();
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/").await, 0);

    // Past the breaker, the first cycle clears the queue.
    clock.advance(16 * 60);
    again.worker.run_once(Instant::now()).await.unwrap();
    assert_eq!(
        rd_calls(&rig, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
    assert!(!again.state.owns(RD_ID));
    assert_eq!(counters(&rig)["pending_delete"], json!([]));
    drop(again.stop);
}

#[tokio::test]
async fn a_job_the_portal_reclaimed_resumes_the_owned_torrent() {
    let mut rig = rig().await;
    mount_json(
        &rig.rd,
        "GET",
        &format!("/torrents/info/{RD_ID}"),
        200,
        info("downloaded", vec![file(1, MOVIE, 4 * GIB, true)], true),
    )
    .await;
    let job = picked_job(&rig).await;

    // An earlier run claimed the job, added the torrent, told the portal,
    // and then died.
    let auth = format!("Bearer {TEST_INTERNAL_TOKEN}");
    let headers = [
        ("Authorization", auth.as_str()),
        ("Content-Type", "application/json"),
    ];
    let claim = rig
        .app
        .internal_request("POST", "/internal/jobs/claim", &headers, None)
        .await;
    assert_eq!(claim.status, 200, "{}", claim.body);
    let claim: Value = serde_json::from_str(&claim.body).unwrap();
    let progress = rig
        .app
        .internal_request(
            "POST",
            &format!("/internal/jobs/{job}/progress"),
            &headers,
            Some(&json!({"claim_id": claim["claim_id"], "rd_torrent_id": RD_ID}).to_string()),
        )
        .await;
    assert_eq!(progress.status, 200, "{}", progress.body);
    rig.state.own(RD_ID, HASH).unwrap();
    rig.app.clock.advance(Duration::from_secs(46 * 60));

    assert_eq!(run(&mut rig).await, Cycle::Worked);

    let row = job_row(&rig.app.pool, job).await;
    assert_eq!(
        (
            row.state.as_str(),
            row.rd_torrent_id.as_deref(),
            row.attempts
        ),
        ("done", Some(RD_ID), 2)
    );
    assert_eq!(rd_calls(&rig, "POST", "/torrents/addMagnet").await, 0);
}

#[tokio::test]
async fn queued_deletions_wait_in_dry_run_and_while_switched_off() {
    let mut rig = rig().await;
    mount_delete(&rig.rd, "OWNED1").await;
    rig.state.own("OWNED1", HASH).unwrap();
    rig.state.pending_delete_add("OWNED1").unwrap();

    for body in [
        r#"{"mode":"dry_run"}"#,
        r#"{"mode":"assisted","enabled":false}"#,
    ] {
        let response = admin_call(&rig.app, &rig.admin, "PUT", "/api/admin/worker", body).await;
        assert_eq!(response.status, 200, "{}", response.body);
        run(&mut rig).await;
        assert!(
            rig.rd.received_requests().await.unwrap().is_empty(),
            "{body}: no Real-Debrid call at all"
        );
        assert_eq!(counters(&rig)["pending_delete"], json!(["OWNED1"]));
    }

    let response = admin_call(
        &rig.app,
        &rig.admin,
        "PUT",
        "/api/admin/worker",
        r#"{"enabled":true}"#,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    run(&mut rig).await;
    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/OWNED1").await, 1);
    assert_eq!(counters(&rig)["pending_delete"], json!([]));
    assert!(!rig.state.owns("OWNED1"));
}

#[tokio::test]
async fn a_queued_deletion_that_fails_again_stays_on_the_list() {
    let mut rig = rig().await;
    Mock::given(method("DELETE"))
        .and(path("/torrents/delete/OWNED1"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&rig.rd)
        .await;
    rig.state.own("OWNED1", HASH).unwrap();
    rig.state.pending_delete_add("OWNED1").unwrap();

    run(&mut rig).await;

    assert_eq!(rd_calls(&rig, "DELETE", "/torrents/delete/OWNED1").await, 1);
    assert_eq!(counters(&rig)["pending_delete"], json!(["OWNED1"]));
    assert!(rig.state.owns("OWNED1"));
}

// ---------------------------------------------------------------------
// Alerts, against a stub portal so each call can be counted
// ---------------------------------------------------------------------

struct StubPortal {
    portal: MockServer,
    rd: MockServer,
    worker: Worker<TorrentioSource>,
    state: StateStore,
    _stop: watch::Sender<bool>,
    _dir: TempDir,
}

fn claim_json() -> Value {
    json!({
        "id": 1, "claim_id": "c-1", "tmdb_id": 603, "imdb_id": IMDB, "media_type": "movie",
        "title": "Example Movie", "year": 1994, "infuse_pro": false,
        "chosen_infohash": HASH, "picked": true, "rd_torrent_id": null,
        "remove_rd_torrent_id": null, "blocked_hashes": [],
    })
}

async fn stub_portal(
    clock: Option<Arc<dyn Clock>>,
    alert_status: u16,
    untrusted_file: bool,
) -> StubPortal {
    stub_portal_with(clock, alert_status, untrusted_file, true).await
}

/// `settings_on` mounts a settings answer that is always "on"; a test that
/// scripts the answers passes false and mounts its own.
async fn stub_portal_with(
    clock: Option<Arc<dyn Clock>>,
    alert_status: u16,
    untrusted_file: bool,
    settings_on: bool,
) -> StubPortal {
    let portal = MockServer::start().await;
    if settings_on {
        mount_json(
            &portal,
            "GET",
            "/internal/settings",
            200,
            json!({"enabled": true, "mode": "assisted"}),
        )
        .await;
    }
    mount_json(&portal, "POST", "/internal/jobs/claim", 200, claim_json()).await;
    mount_json(
        &portal,
        "POST",
        "/internal/jobs/1/progress",
        200,
        json!({"ok": true}),
    )
    .await;
    mount_json(
        &portal,
        "POST",
        "/internal/jobs/1/report",
        200,
        json!({"ok": true}),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/internal/alert"))
        .respond_with(ResponseTemplate::new(alert_status).set_body_json(json!({"ok": true})))
        .mount(&portal)
        .await;
    let rd = MockServer::start().await;
    let stub = MockServer::start().await;
    let dir = state_dir();
    if untrusted_file {
        fs::write(dir.0.join("counters.json"), b"{not json").unwrap();
    }
    let built = build_worker(&portal.uri(), &rd, &stub, &dir.0, clock, timing());
    StubPortal {
        portal,
        rd,
        worker: built.worker,
        state: built.state,
        _stop: built.stop,
        _dir: dir,
    }
}

impl StubPortal {
    async fn run(&mut self) -> Cycle {
        self.worker.run_once(Instant::now()).await.unwrap()
    }

    async fn posted(&self, route: &str) -> Vec<Value> {
        self.portal
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "POST" && r.url.path() == route)
            .map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
            .collect()
    }

    async fn alerts(&self) -> Vec<String> {
        self.posted("/internal/alert")
            .await
            .iter()
            .map(|b| b["kind"].as_str().unwrap().to_string())
            .collect()
    }

    /// Every add is refused by Real-Debrid.
    async fn refuse_adds(&self) {
        self.rd.reset().await;
        mount_json(
            &self.rd,
            "GET",
            "/torrents/activeCount",
            200,
            json!({"nb": 0, "limit": 25}),
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/torrents"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&self.rd)
            .await;
        mount_json(
            &self.rd,
            "POST",
            "/torrents/addMagnet",
            403,
            json!({"error_code": 35}),
        )
        .await;
    }
}

#[tokio::test]
async fn three_failed_runs_in_a_row_raise_one_alert_and_a_result_starts_the_count_over() {
    let mut mp = stub_portal(None, 200, false).await;
    mp.refuse_adds().await;
    for _ in 0..5 {
        assert_eq!(mp.run().await, Cycle::Worked);
    }
    assert_eq!(mp.alerts().await, ["repeated_failures"]);

    // A result resets the count.
    mp.rd.reset().await;
    mount_roomy(&mp.rd).await;
    mount_cached_torrent(&mp.rd).await;
    mp.run().await;
    let reports = mp.posted("/internal/jobs/1/report").await;
    assert_eq!(reports.last().unwrap()["state"], "done");

    mp.refuse_adds().await;
    mp.run().await;
    mp.run().await;
    assert_eq!(
        mp.alerts().await,
        ["repeated_failures"],
        "two failures are not three"
    );
    mp.run().await;
    assert_eq!(
        mp.alerts().await,
        ["repeated_failures", "repeated_failures"]
    );
}

async fn rate_limited_active_count(rd: &MockServer) {
    mount_json(
        rd,
        "GET",
        "/torrents/activeCount",
        429,
        json!({"error": "too_many_requests", "error_code": 34}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(204))
        .mount(rd)
        .await;
}

#[tokio::test]
async fn each_breaker_trip_raises_exactly_one_alert() {
    let clock = TestClock::new();
    let mut mp = stub_portal(Some(clock.clone()), 200, false).await;
    rate_limited_active_count(&mp.rd).await;

    assert_eq!(mp.run().await, Cycle::Worked);
    assert_eq!(mp.alerts().await, ["breaker_open"]);
    let reports = mp.posted("/internal/jobs/1/report").await;
    assert_eq!(reports[0]["reason"], "rate_limited");

    // Held off: nothing is claimed, and nothing more is said.
    for _ in 0..3 {
        assert_eq!(mp.run().await, Cycle::Wait(POLL));
    }
    assert_eq!(mp.alerts().await, ["breaker_open"]);
    assert_eq!(mp.posted("/internal/jobs/claim").await.len(), 1);

    // A second trip after the first has passed is a second alert.
    clock.advance(16 * 60);
    assert_eq!(mp.run().await, Cycle::Worked);
    assert_eq!(mp.alerts().await, ["breaker_open", "breaker_open"]);
}

#[tokio::test]
async fn a_rejected_token_raises_rd_auth_once_and_stops_claiming() {
    let mut mp = stub_portal(None, 200, false).await;
    mount_json(
        &mp.rd,
        "GET",
        "/torrents/activeCount",
        401,
        json!({"error": "bad_token", "error_code": 8}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/torrents"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mp.rd)
        .await;
    assert_eq!(mp.run().await, Cycle::Worked);
    for _ in 0..3 {
        assert_eq!(mp.run().await, Cycle::Wait(POLL));
    }
    assert_eq!(mp.alerts().await, ["rd_auth"]);
    assert_eq!(mp.posted("/internal/jobs/claim").await.len(), 1);
    assert!(!format!("{:?}", mp.portal.received_requests().await.unwrap()).contains("test-token"));
}

#[tokio::test]
async fn an_untrusted_counters_file_raises_its_alert_once_and_claims_nothing() {
    let mut mp = stub_portal(None, 200, true).await;
    assert!(mp.state.unreadable());
    for _ in 0..3 {
        assert_eq!(mp.run().await, Cycle::Wait(POLL));
    }
    assert_eq!(mp.alerts().await, ["counters_unreadable"]);
    assert!(mp.posted("/internal/jobs/claim").await.is_empty());
    assert!(mp.rd.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failing_alert_call_changes_nothing_about_the_run() {
    let mut mp = stub_portal(None, 503, false).await;
    rate_limited_active_count(&mp.rd).await;
    assert_eq!(mp.run().await, Cycle::Worked);
    let reports = mp.posted("/internal/jobs/1/report").await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["reason"], "rate_limited");
    assert_eq!(mp.alerts().await, ["breaker_open"]);
}

// ---------------------------------------------------------------------
// The kill switch, read in a script (no timing involved)
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Switch {
    On,
    Off,
    Down,
}

/// The portal's settings answers in order: each step for its number of
/// reads, the last one for every read after that.
async fn script_settings(portal: &MockServer, steps: &[(Switch, u64)]) {
    for (i, (answer, reads)) in steps.iter().enumerate() {
        let response = match answer {
            Switch::On => ResponseTemplate::new(200)
                .set_body_json(json!({"enabled": true, "mode": "assisted"})),
            Switch::Off => ResponseTemplate::new(200)
                .set_body_json(json!({"enabled": false, "mode": "assisted"})),
            Switch::Down => ResponseTemplate::new(500),
        };
        let mock = Mock::given(method("GET"))
            .and(path("/internal/settings"))
            .respond_with(response);
        if i + 1 < steps.len() {
            mock.up_to_n_times(*reads).mount(portal).await;
        } else {
            mock.mount(portal).await;
        }
    }
}

/// One file list, then `downloading` for `waits` looks, then done.
async fn mount_slow_torrent(rd: &MockServer, waits: u64) {
    let waiting = info(
        "waiting_files_selection",
        vec![file(1, MOVIE, 4 * GIB, false)],
        false,
    );
    Mock::given(method("GET"))
        .and(path(format!("/torrents/info/{RD_ID}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(waiting))
        .up_to_n_times(1)
        .mount(rd)
        .await;
    let downloading = info("downloading", vec![file(1, MOVIE, 4 * GIB, true)], false);
    if waits > 0 {
        Mock::given(method("GET"))
            .and(path(format!("/torrents/info/{RD_ID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(downloading))
            .up_to_n_times(waits)
            .mount(rd)
            .await;
    }
    let done = info("downloaded", vec![file(1, MOVIE, 4 * GIB, true)], true);
    mount_json(rd, "GET", &format!("/torrents/info/{RD_ID}"), 200, done).await;
    Mock::given(method("POST"))
        .and(path(format!("/torrents/selectFiles/{RD_ID}")))
        .respond_with(ResponseTemplate::new(204))
        .mount(rd)
        .await;
    mount_delete(rd, RD_ID).await;
}

async fn last_report(mp: &StubPortal) -> Value {
    mp.posted("/internal/jobs/1/report")
        .await
        .pop()
        .expect("a report was sent")
}

// Reads of the settings in one picked run: the loop (1), the gate at the
// start (2), the look before the add (3), the look before the selection
// (4), then one per poll.

#[tokio::test]
async fn a_switch_turned_off_just_before_the_add_adds_nothing() {
    let mut mp = stub_portal_with(None, 200, false, false).await;
    script_settings(&mp.portal, &[(Switch::On, 2), (Switch::Off, 0)]).await;
    mount_roomy(&mp.rd).await;

    assert_eq!(mp.run().await, Cycle::Worked);

    let report = last_report(&mp).await;
    assert_eq!(report["reason"], "worker_disabled");
    assert_eq!(rd_calls_on(&mp.rd, "POST", "/torrents/addMagnet").await, 0);
    assert_eq!(mp.state.adds_left(), 50, "no add was counted");
}

#[tokio::test]
async fn a_switch_turned_off_between_the_add_and_the_selection_deletes_the_torrent() {
    let mut mp = stub_portal_with(None, 200, false, false).await;
    script_settings(&mp.portal, &[(Switch::On, 3), (Switch::Off, 0)]).await;
    mount_roomy(&mp.rd).await;
    mount_slow_torrent(&mp.rd, 0).await;

    assert_eq!(mp.run().await, Cycle::Worked);

    assert_eq!(last_report(&mp).await["reason"], "worker_disabled");
    assert_eq!(rd_calls_on(&mp.rd, "POST", "/torrents/addMagnet").await, 1);
    assert_eq!(
        rd_calls_on(&mp.rd, "POST", "/torrents/selectFiles/").await,
        0
    );
    assert_eq!(
        rd_calls_on(&mp.rd, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
    assert!(!mp.state.owns(RD_ID));
}

#[tokio::test]
async fn an_unreadable_switch_before_the_selection_fails_closed() {
    let mut mp = stub_portal_with(None, 200, false, false).await;
    script_settings(
        &mp.portal,
        &[(Switch::On, 3), (Switch::Down, 1), (Switch::On, 0)],
    )
    .await;
    mount_roomy(&mp.rd).await;
    mount_slow_torrent(&mp.rd, 0).await;

    assert_eq!(mp.run().await, Cycle::Worked);

    assert_eq!(last_report(&mp).await["reason"], "rd_error");
    assert_eq!(
        rd_calls_on(&mp.rd, "POST", "/torrents/selectFiles/").await,
        0
    );
    assert_eq!(
        rd_calls_on(&mp.rd, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
}

#[tokio::test]
async fn three_failed_reads_in_a_row_while_waiting_are_put_up_with() {
    let mut mp = stub_portal_with(None, 200, false, false).await;
    // Reads 5 to 7 fail, 8 and on work.
    script_settings(
        &mp.portal,
        &[(Switch::On, 4), (Switch::Down, 3), (Switch::On, 0)],
    )
    .await;
    mount_roomy(&mp.rd).await;
    mount_slow_torrent(&mp.rd, 3).await;

    assert_eq!(mp.run().await, Cycle::Worked);

    let report = last_report(&mp).await;
    assert_eq!(report["state"], "done", "{report}");
    assert_eq!(rd_calls_on(&mp.rd, "DELETE", "/torrents/delete/").await, 0);
}

#[tokio::test]
async fn a_fourth_failed_read_in_a_row_while_waiting_ends_the_run_and_deletes_the_torrent() {
    let mut mp = stub_portal_with(None, 200, false, false).await;
    script_settings(
        &mp.portal,
        &[(Switch::On, 4), (Switch::Down, 4), (Switch::On, 0)],
    )
    .await;
    mount_roomy(&mp.rd).await;
    mount_slow_torrent(&mp.rd, 20).await;

    assert_eq!(mp.run().await, Cycle::Worked);

    assert_eq!(last_report(&mp).await["reason"], "rd_error");
    assert_eq!(
        rd_calls_on(&mp.rd, "DELETE", &format!("/torrents/delete/{RD_ID}")).await,
        1
    );
    assert!(!mp.state.owns(RD_ID));
}

#[tokio::test]
async fn a_good_read_resets_the_count_of_failed_ones() {
    let mut mp = stub_portal_with(None, 200, false, false).await;
    // 3 failures, one good read, 3 failures again: never four in a row.
    script_settings(
        &mp.portal,
        &[
            (Switch::On, 4),
            (Switch::Down, 3),
            (Switch::On, 1),
            (Switch::Down, 3),
            (Switch::On, 0),
        ],
    )
    .await;
    mount_roomy(&mp.rd).await;
    mount_slow_torrent(&mp.rd, 6).await;

    assert_eq!(mp.run().await, Cycle::Worked);

    assert_eq!(last_report(&mp).await["state"], "done");
}

async fn rd_calls_on(rd: &MockServer, verb: &str, route_prefix: &str) -> usize {
    rd.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == verb && r.url.path().starts_with(route_prefix))
        .count()
}
