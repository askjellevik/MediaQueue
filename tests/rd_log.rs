//! What the Real-Debrid client logs about a failed call: the HTTP status and
//! the numeric error code, never the body and never the token. Own binary
//! because it installs a global log subscriber (see throttle_alert.rs).

use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use mediaqueue::worker::{
    rd::{RdClient, RdError, Secret},
    state::{StateStore, SystemClock},
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const TOKEN: &str = "test-token-0000";

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
async fn a_refused_call_logs_status_and_code_but_not_the_body_or_the_token() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .init();

    let dir = std::env::temp_dir().join(format!("mediaqueue-rd-log-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/torrents/addMagnet"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": format!("Example.Movie.1994.1080p.WEB-DL.AAC is infringing, {TOKEN}"),
            "error_code": 35,
        })))
        .mount(&server)
        .await;

    let state = StateStore::open(&dir, Arc::new(SystemClock));
    let client = RdClient::new(&server.uri(), &Secret::new(TOKEN), state, 600).unwrap();
    let result = client.add_magnet(&"a".repeat(40)).await;
    assert_eq!(result, Err(RdError::Refused(35)));

    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("status 403"), "{text}");
    assert!(text.contains("error_code 35"), "{text}");
    assert!(!text.contains("test-token"), "{text}");
    assert!(!text.contains("Example.Movie"), "{text}");
    assert!(!text.contains("infringing"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}
