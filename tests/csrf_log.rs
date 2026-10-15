//! The CSRF guard's refusals: every failing combination is still a 403 with
//! the same body, and the log says which check failed and what Origin
//! arrived. Own binary because it installs a global log subscriber.

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

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
async fn every_failing_combination_is_403_and_logged_with_its_reason() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .init();

    let app = common::spawn_app().await;
    let mut bodies = Vec::new();

    for mask in 0u8..8 {
        let good_origin = mask & 1 != 0;
        let good_xrw = mask & 2 != 0;
        let good_type = mask & 4 != 0;
        let origin = if good_origin {
            "https://example.com"
        } else {
            "http://example.com:8080"
        };
        let mut headers = vec![("Origin", origin)];
        if good_xrw {
            headers.push(("X-Requested-With", "mp"));
        }
        headers.push((
            "Content-Type",
            if good_type {
                "application/json"
            } else {
                "text/plain"
            },
        ));
        // A distinct path per combination finds its own log line.
        let path = format!("/api/auth/code?case={mask}");
        let response = app
            .request("POST", &path, &headers, Some(r#"{"code":"x"}"#))
            .await;
        if mask == 7 {
            assert_ne!(response.status, 403, "the all-good request was refused");
        } else {
            assert_eq!(response.status, 403, "mask {mask}: {}", response.body);
            bodies.push(response.body);
        }
    }
    assert!(
        bodies.windows(2).all(|w| w[0] == w[1]),
        "the 403 body must not reveal which check failed: {bodies:?}"
    );

    tokio::time::sleep(Duration::from_millis(50)).await;
    let text = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    let rejected: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("rejected by the csrf guard"))
        .collect();
    assert_eq!(rejected.len(), 7, "{text}");
    for (reason, count) in [
        ("origin_mismatch", 4),
        ("missing_x_requested_with", 4),
        ("bad_content_type", 4),
    ] {
        assert_eq!(
            rejected.iter().filter(|l| l.contains(reason)).count(),
            count,
            "{reason}"
        );
    }
    // The bad origin is logged as received, so a PUBLIC_ORIGIN mistake shows.
    assert!(
        rejected
            .iter()
            .any(|l| l.contains("http://example.com:8080") && l.contains("https://example.com"))
    );
}
