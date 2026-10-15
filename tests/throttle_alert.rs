//! The global throttle turning on must log a warning and send exactly one
//! ntfy alert. Lives in its own test binary because it installs a global
//! log subscriber: a thread-local one races with tracing's callsite cache
//! when other tests log from parallel threads.

mod common;

use std::{
    io::Write,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{HttpResponse, TestApp, create_member, json_post_headers};
use mediaqueue::config::NtfyConfig;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, header, method, path},
};

/// Collects everything the app logs.
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

impl LogCapture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

async fn post_code(app: &TestApp, client_ip: &str, code: &str) -> HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("X-Real-IP", client_ip));
    let body = format!(r#"{{"code":"{code}"}}"#);
    app.request("POST", "/api/auth/code", &headers, Some(&body))
        .await
}

fn nth_ip(n: u32) -> String {
    Ipv4Addr::from_bits(0xc633_6400 + n).to_string() // 198.51.100.0 + n
}

#[tokio::test]
async fn throttle_turning_on_logs_a_warning_and_sends_exactly_one_alert() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .init();

    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .and(header("authorization", "Bearer ntfy-test-token"))
        .and(body_partial_json(serde_json::json!({
            "topic": "throttle-test-topic",
            "title": "Access code throttle on",
            "tags": ["warning"],
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&ntfy)
        .await;

    let ntfy_uri = ntfy.uri();
    let app = common::spawn_app_with(|cfg| {
        cfg.ntfy = NtfyConfig {
            url: Some(ntfy_uri),
            topic: "throttle-test-topic".into(),
            token: Some("ntfy-test-token".into()),
        };
    })
    .await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;

    // 80 wrong codes over HTTP, each from its own IP: the throttle turns on
    // at the 61st and stays on.
    for n in 0..80 {
        assert_eq!(post_code(&app, &nth_ip(n), "ZZZZ-ZZZZ").await.status, 401);
    }
    assert_eq!(post_code(&app, &nth_ip(200), &code).await.status, 200);

    // The alert goes out on a background task: wait (in real time, the
    // network is real) for it to land, then a little longer to be sure no
    // second one follows.
    for _ in 0..100 {
        let received = ntfy.received_requests().await.unwrap_or_default();
        if !received.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    ntfy.verify().await;

    let logs = logs.text();
    let warnings: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("global login throttle turned on"))
        .collect();
    assert_eq!(warnings.len(), 1, "logs were:\n{logs}");
    assert!(warnings[0].contains(r#""level":"WARN""#));
    assert!(warnings[0].contains(r#""failures":61"#));
    // Nothing secret reaches the log.
    assert!(!logs.contains(&code));
    assert!(!logs.contains("ntfy-test-token"));
    assert!(!logs.contains("throttle-test-topic"));
}
