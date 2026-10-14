//! The state annotation on search results must cost one SELECT however
//! many results there are. Counted from sqlx's own statement log, so this
//! binary installs a global log subscriber (see throttle_alert.rs for why
//! that needs its own test binary).

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{insert_request, login_new_member};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
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

impl LogCapture {
    fn take(&self) -> String {
        let mut buf = self.0.lock().unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        buf.clear();
        text
    }
}

fn movies(count: u32) -> Value {
    let results: Vec<Value> = (1..=count)
        .map(|id| json!({ "id": id, "media_type": "movie", "title": format!("Movie {id}") }))
        .collect();
    json!({ "results": results })
}

/// Statements that read the `requests` table, from sqlx's debug log.
fn request_selects(logs: &str) -> usize {
    logs.lines()
        .filter(|l| l.contains("sqlx::query") && l.contains("FROM requests"))
        .count()
}

#[tokio::test]
async fn state_annotation_is_one_select_regardless_of_result_count() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .json()
        .with_env_filter("sqlx::query=debug")
        .with_writer(move || writer.clone())
        .init();

    let server = MockServer::start().await;
    for (query, count) in [("one", 1), ("three", 3), ("many", 20)] {
        Mock::given(method("GET"))
            .and(path("/search/multi"))
            .and(query_param("query", query))
            .respond_with(ResponseTemplate::new(200).set_body_json(movies(count)))
            .mount(&server)
            .await;
    }
    let uri = server.uri();
    let app = common::spawn_app_with(move |cfg| cfg.tmdb_base_url = uri).await;
    let cookie = login_new_member(&app, "Alice").await;
    let (bob, _) = common::create_member(&app.pool, &app.config, "Bob").await;
    for id in 1..=5 {
        insert_request(&app.pool, bob, id, "movie", "pending").await;
    }
    // Let earlier statements reach the log, then start counting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    logs.take();

    for (query, expected_results) in [("one", 1), ("three", 3), ("many", 8)] {
        let response = app
            .request(
                "GET",
                &format!("/api/search?q={query}"),
                &[("Cookie", &cookie)],
                None,
            )
            .await;
        assert_eq!(response.status, 200, "{}", response.body);
        let body: Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["results"].as_array().unwrap().len(), expected_results);

        let captured = logs.take();
        assert_eq!(
            request_selects(&captured),
            1,
            "query {query:?} ran these statements:\n{captured}"
        );
    }
}
