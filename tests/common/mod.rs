//! Shared integration test helpers: spin up the real app on a random port
//! and talk to it with a tiny stdlib HTTP client (no reqwest until Phase 3).

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use media_requests::{build_router, config::Config, db, state::AppState};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

pub const TEST_ADMIN_USER: &str = "admin";
pub const TEST_ADMIN_PASSWORD: &str = "test-admin-password";

/// A real Argon2id PHC hash of [`TEST_ADMIN_PASSWORD`], computed once.
pub fn test_admin_password_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        use argon2::password_hash::PasswordHasher;
        argon2::Argon2::default()
            .hash_password(TEST_ADMIN_PASSWORD.as_bytes())
            .expect("hash the fixed test admin password")
            .to_string()
    })
}

/// A minimal raw-socket HTTP/1.1 GET. Returns the full response text
/// (status line, headers and body) so tests can assert on any of it.
///
/// Runs on a blocking-pool thread so the synchronous socket I/O never
/// starves the single test task's runtime, which would otherwise never get
/// a chance to poll the spawned server and accept the connection.
pub async fn http_get(addr: SocketAddr, path: &str) -> String {
    let path = path.to_string();
    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).expect("connect to test server");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("set read timeout");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).expect("write request");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read response");
        String::from_utf8_lossy(&buf).into_owned()
    })
    .await
    .expect("blocking http_get task panicked")
}

/// Builds a fresh `Config` pointed at a unique temp-file database, bypassing
/// `Config::from_env` so tests don't touch process environment variables.
fn test_config() -> Config {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let db_path = std::env::temp_dir().join(format!("media-requests-test-{nanos}-{n}.sqlite3"));

    Config {
        public_origin: "https://example.com".into(),
        bind_addr: "127.0.0.1:0".parse().expect("valid socket addr"),
        database_path: db_path,
        code_pepper: b"integration-test-pepper-at-least-32-bytes!!".to_vec(),
        cookie_dev_insecure: false,
        code_length: 8,
        default_max_sessions: 3,
        default_daily_limit: 10,
        admin_user: TEST_ADMIN_USER.to_string(),
        admin_password_hash: test_admin_password_hash().to_string(),
        trusted_proxy_cidrs: media_requests::clientip::parse_cidr_list(
            "127.0.0.1/32,::1/128,172.16.0.0/12",
        )
        .expect("valid default trusted proxy cidrs"),
    }
}

/// Starts the real app (router, DB pool, migrations) on an OS-assigned
/// loopback port and returns the address it is listening on.
pub async fn spawn_app() -> SocketAddr {
    let config = test_config();
    let pool = db::connect(&config)
        .await
        .expect("connect and migrate test database");
    let state = AppState::new(pool, config);
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random port");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("test server crashed");
    });

    addr
}
