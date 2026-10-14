//! The structured request log: one line per request with method, path,
//! status, latency and IP, and nothing sensitive. Own binary because it
//! installs a global log subscriber (see throttle_alert.rs).

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{TEST_ADMIN_PASSWORD, TEST_ADMIN_USER, cookie_pair, json_post_headers};

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn requests_are_logged_without_secrets() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .init();

    let app = common::spawn_app().await;
    let (_, code) = common::create_member(&app.pool, &app.config, "Alice").await;
    let mut headers = json_post_headers();
    headers.push(("X-Real-IP", "198.51.100.7"));
    let login = app
        .request(
            "POST",
            "/api/auth/code",
            &headers,
            Some(&format!(r#"{{"code":"{code}"}}"#)),
        )
        .await;
    let cookie = cookie_pair(login.header("set-cookie").unwrap()).to_string();
    let token = cookie.split('=').nth(1).unwrap().to_string();
    app.request(
        "GET",
        "/api/search?q=secret+query",
        &[("Cookie", &cookie)],
        None,
    )
    .await;
    let admin_body =
        format!(r#"{{"username":"{TEST_ADMIN_USER}","password":"{TEST_ADMIN_PASSWORD}"}}"#);
    app.request(
        "POST",
        "/api/admin/login",
        &json_post_headers(),
        Some(&admin_body),
    )
    .await;
    app.request("GET", "/healthz", &[], None).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let text = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains(r#""target":"media_requests::request""#))
        .collect();
    let login_line = lines
        .iter()
        .find(|l| l.contains(r#""path":"/api/auth/code""#))
        .expect("login logged");
    for field in [
        r#""method":"POST""#,
        r#""status":200"#,
        r#""ip":"198.51.100.7""#,
        r#""ms":"#,
    ] {
        assert!(login_line.contains(field), "{field} missing: {login_line}");
    }
    let search_line = lines
        .iter()
        .find(|l| l.contains("/api/search"))
        .expect("search logged");
    assert!(search_line.contains(r#""path":"/api/search""#));
    assert!(
        lines.iter().all(|l| !l.contains("/healthz")),
        "health checks are not logged"
    );

    // Nothing secret anywhere in the log.
    for secret in [
        code.as_str(),
        token.as_str(),
        TEST_ADMIN_PASSWORD,
        "secret query",
        "secret+query",
    ] {
        assert!(!text.contains(secret), "log contains {secret:?}:\n{text}");
    }
}
