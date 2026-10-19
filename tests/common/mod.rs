//! Shared integration test helpers: spin up the real app on a random port
//! and talk to it with a tiny stdlib HTTP client, so tests see exactly the
//! bytes the server sent.
//!
//! Each `tests/*.rs` file compiles this module separately as its own copy,
//! so any one binary using only part of this API is expected, not dead code.
#![allow(dead_code)]

use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use mediaqueue::{
    build_router, clock::Clock, config::Config, db, internal_router, state::AppState,
};
use sqlx::SqlitePool;

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

pub const TEST_ADMIN_USER: &str = "admin";
pub const TEST_ADMIN_PASSWORD: &str = "test-admin-password";
pub const TEST_TMDB_TOKEN: &str = "test-tmdb-token";
pub const TEST_INTERNAL_TOKEN: &str = "test-internal-token-0123456789abcdef-xyz";

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

/// A parsed HTTP/1.1 response: status code, headers (in order, duplicates
/// kept) and body text.
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    /// First header matching `name`, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every header matching `name`, case-insensitively (a response may set
    /// more than one `Set-Cookie`, for example).
    pub fn headers_all(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// The `name=value` pair from a `Set-Cookie` header value, discarding
/// attributes, ready to send back in a `Cookie:` request header.
pub fn cookie_pair(set_cookie_value: &str) -> &str {
    set_cookie_value.split(';').next().unwrap_or("").trim()
}

fn parse_http_response(raw: &str) -> HttpResponse {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let headers = lines
        .filter_map(|l| {
            l.split_once(": ")
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    HttpResponse {
        status,
        headers,
        body: body.to_string(),
    }
}

/// A minimal raw-socket HTTP/1.1 request. Runs on a blocking-pool thread so
/// the synchronous socket I/O never starves the single test task's
/// runtime, which would otherwise never get a chance to poll the spawned
/// server and accept the connection.
pub async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    extra_headers: &[(&str, &str)],
    body: Option<&str>,
) -> HttpResponse {
    let method = method.to_string();
    let path = path.to_string();
    let extra_headers: Vec<(String, String)> = extra_headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let body = body.map(str::to_string);

    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(addr).expect("connect to test server");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set read timeout");

        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        for (k, v) in &extra_headers {
            request.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = &body {
            request.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        request.push_str("\r\n");
        if let Some(b) = &body {
            request.push_str(b);
        }

        stream.write_all(request.as_bytes()).expect("write request");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).expect("read response");
        parse_http_response(&String::from_utf8_lossy(&buf))
    })
    .await
    .expect("blocking http_request task panicked")
}

/// Convenience GET, used by the healthz and static-file tests.
pub async fn http_get(addr: SocketAddr, path: &str) -> String {
    let response = http_request(addr, "GET", path, &[], None).await;
    let mut raw = format!("HTTP/1.1 {} \r\n", response.status);
    for (k, v) in &response.headers {
        raw.push_str(&format!("{k}: {v}\r\n"));
    }
    raw.push_str("\r\n");
    raw.push_str(&response.body);
    raw
}

/// The standard headers a browser's `fetch` sends for a same-origin JSON
/// POST, matching what the CSRF guard requires.
pub fn json_post_headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Origin", "https://example.com"),
        ("X-Requested-With", "mp"),
        ("Content-Type", "application/json"),
    ]
}

/// Builds a fresh `Config` pointed at a unique temp-file database, bypassing
/// `Config::from_env` so tests don't touch process environment variables.
fn test_config() -> Config {
    let n = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let db_path = std::env::temp_dir().join(format!("mediaqueue-test-{nanos}-{n}.sqlite3"));

    Config {
        public_origin: "https://example.com".into(),
        bind_addr: "127.0.0.1:0".parse().expect("valid socket addr"),
        database_path: db_path,
        code_pepper: b"integration-test-pepper-at-least-32-bytes!!".to_vec(),
        cookie_dev_insecure: false,
        admin_user: TEST_ADMIN_USER.to_string(),
        admin_password_hash: test_admin_password_hash().to_string(),
        trusted_proxy_cidrs: mediaqueue::clientip::parse_cidr_list(
            "127.0.0.1/32,::1/128,172.16.0.0/12",
        )
        .expect("valid default trusted proxy cidrs"),
        tmdb_token: TEST_TMDB_TOKEN.to_string(),
        // Nothing listens here; tests that search point this at wiremock.
        tmdb_base_url: "http://127.0.0.1:9".into(),
        dmm_base_url: "https://dmm.example".into(),
        cf_access: None,
        ntfy: Default::default(),
        rate_limits: Default::default(),
        internal_listen: Some("127.0.0.1:0".parse().expect("valid socket addr")),
        internal_token: Some(TEST_INTERNAL_TOKEN.to_string()),
        worker_enabled: None,
        worker_mode: None,
    }
}

pub struct TestApp {
    pub addr: SocketAddr,
    /// The worker's non-public listener.
    pub internal_addr: SocketAddr,
    pub pool: SqlitePool,
    pub config: Config,
    /// The app's shared state, for reaching into limiters directly.
    pub state: AppState,
    /// The fake clock every time-based limit in the app reads.
    pub clock: Clock,
}

impl TestApp {
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> HttpResponse {
        http_request(self.addr, method, path, extra_headers, body).await
    }

    /// Like [`TestApp::request`], against the internal listener.
    pub async fn internal_request(
        &self,
        method: &str,
        path: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> HttpResponse {
        http_request(self.internal_addr, method, path, extra_headers, body).await
    }
}

/// Starts the real app (router, DB pool, migrations) on an OS-assigned
/// loopback port with the default test config.
pub async fn spawn_app() -> TestApp {
    spawn_app_with(|_| {}).await
}

/// Like [`spawn_app`], but lets the caller tweak the config first (e.g. a
/// different rate limits, or `cookie_dev_insecure = true`).
pub async fn spawn_app_with(customize: impl FnOnce(&mut Config)) -> TestApp {
    spawn_app_custom(customize, |_| {}).await
}

/// Like [`spawn_app_with`], and also lets the caller adjust the app state
/// (for example a quicker event-stream heartbeat) before the router is built.
pub async fn spawn_app_custom(
    customize: impl FnOnce(&mut Config),
    customize_state: impl FnOnce(&mut AppState),
) -> TestApp {
    let mut config = test_config();
    customize(&mut config);

    let pool = db::connect(&config)
        .await
        .expect("connect and migrate test database");
    let clock = Clock::fake(1_700_000_000);
    let mut state =
        AppState::with_clock(pool.clone(), config.clone(), clock.clone()).expect("build app state");
    customize_state(&mut state);
    let app = build_router(state.clone());
    let internal_app = internal_router(state.clone());

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

    let internal_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random internal port");
    let internal_addr = internal_listener.local_addr().expect("internal local addr");
    tokio::spawn(async move {
        axum::serve(
            internal_listener,
            internal_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("internal test server crashed");
    });

    TestApp {
        addr,
        internal_addr,
        pool,
        config,
        state,
        clock,
    }
}

/// Creates a member directly in the database (bypassing HTTP) and returns
/// `(member_id, raw_code)`. Mirrors what the `add-member` CLI does.
pub async fn create_member(pool: &SqlitePool, config: &Config, name: &str) -> (i64, String) {
    let code = mediaqueue::auth::generate_code(mediaqueue::auth::GENERATED_CODE_LENGTH)
        .expect("os random source");
    let normalised = mediaqueue::auth::normalise_code(&code);
    let code_hash = mediaqueue::auth::hmac_code(&config.code_pepper, &normalised);
    let now = db::now_unix();
    let id = db::insert_member(pool, name, &code_hash, None, now)
        .await
        .expect("insert test member");
    (id, code)
}

/// Creates a member and logs them in over HTTP; returns the `name=value`
/// session cookie to send back.
pub async fn login_new_member(app: &TestApp, name: &str) -> String {
    let (_, code) = create_member(&app.pool, &app.config, name).await;
    let body = format!(r#"{{"code":"{code}"}}"#);
    let response = app
        .request("POST", "/api/auth/code", &json_post_headers(), Some(&body))
        .await;
    assert_eq!(response.status, 200, "login failed: {}", response.body);
    let set_cookie = response.header("set-cookie").expect("session cookie");
    cookie_pair(set_cookie).to_string()
}

/// Inserts a request row straight into the database, for tests that need
/// existing requests without going through the endpoint.
pub async fn insert_request(
    pool: &SqlitePool,
    member_id: i64,
    tmdb_id: i64,
    media_type: &str,
    status: &str,
) {
    sqlx::query(
        "INSERT INTO requests (tmdb_id, media_type, title, status, requested_by, created_at, status_changed_at)
         VALUES (?, ?, 'Seeded', ?, ?, 0, 0)",
    )
    .bind(tmdb_id)
    .bind(media_type)
    .bind(status)
    .bind(member_id)
    .execute(pool)
    .await
    .expect("insert test request");
}

/// Logs in as the admin and returns the `name=value` session cookie.
pub async fn login_admin_cookie(app: &TestApp) -> String {
    let body = format!(r#"{{"username":"{TEST_ADMIN_USER}","password":"{TEST_ADMIN_PASSWORD}"}}"#);
    let response = app
        .request(
            "POST",
            "/api/admin/login",
            &json_post_headers(),
            Some(&body),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    cookie_pair(response.header("set-cookie").expect("admin cookie")).to_string()
}

/// A long-lived GET (server-sent events). Body lines arrive on a channel
/// from a background thread, so a test can wait for one with a timeout,
/// and can tell when the server ended the stream. Dropping it closes the
/// connection.
pub struct EventStream {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    lines: tokio::sync::mpsc::UnboundedReceiver<String>,
    _socket: TcpStream,
}

impl EventStream {
    pub async fn open(addr: SocketAddr, path: &str, cookie: Option<&str>) -> EventStream {
        let path = path.to_string();
        let cookie = cookie.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, BufReader};
            let socket = TcpStream::connect(addr).expect("connect to test server");
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("set read timeout");
            let mut request =
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n");
            if let Some(cookie) = &cookie {
                request.push_str(&format!("Cookie: {cookie}\r\n"));
            }
            request.push_str("\r\n");
            (&socket)
                .write_all(request.as_bytes())
                .expect("write request");

            let mut reader = BufReader::new(socket.try_clone().expect("clone socket"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("status line");
            let status = line
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.parse().ok())
                .expect("status code");
            let mut headers = Vec::new();
            loop {
                line.clear();
                reader.read_line(&mut line).expect("header line");
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    break;
                }
                if let Some((k, v)) = trimmed.split_once(':') {
                    headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                }
            }

            let (tx, lines) = tokio::sync::mpsc::unbounded_channel();
            std::thread::spawn(move || {
                // Read timeouts are not an end; only EOF or an error is.
                socket_lines(reader, tx);
            });
            EventStream {
                status,
                headers,
                lines,
                _socket: socket,
            }
        })
        .await
        .expect("open task panicked")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Waits up to `wait` for a body line containing `needle`.
    pub async fn saw(&mut self, needle: &str, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.lines.recv()).await {
                Ok(Some(line)) if line.contains(needle) => return true,
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return false,
            }
        }
    }

    /// Whether the server ended the stream within `wait`.
    pub async fn ended(&mut self, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.lines.recv()).await {
                // "0" is the last chunk of the body; the connection itself may
                // stay open for reuse.
                Ok(Some(line)) if line == "0" => return true,
                Ok(Some(_)) => {}
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }
}

fn socket_lines(
    mut reader: std::io::BufReader<TcpStream>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    use std::io::BufRead;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {
                if tx.send(line.trim_end().to_string()).is_err() {
                    return;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
    }
}
