//! What the internal listener writes to the log at the default `info`
//! level: the worker's routine polling stays out of it, anything else stays
//! in, and nothing the worker sends or receives is in it. Own binary because
//! it installs a global log subscriber (see request_log.rs).

mod common;

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use common::{TEST_INTERNAL_TOKEN, json_post_headers, login_admin_cookie};
use serde_json::{Value, json};

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

async fn worker(
    app: &common::TestApp,
    method: &str,
    path: &str,
    token: &str,
    body: Option<&Value>,
) -> common::HttpResponse {
    let auth = format!("Bearer {token}");
    let body = body.map(Value::to_string);
    app.internal_request(
        method,
        path,
        &[
            ("Authorization", auth.as_str()),
            ("Content-Type", "application/json"),
        ],
        body.as_deref(),
    )
    .await
}

#[tokio::test]
async fn routine_polling_is_not_logged_at_info_and_nothing_secret_is() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::INFO)
        .with_writer(move || writer.clone())
        .init();

    let app = common::spawn_app().await;
    let admin = login_admin_cookie(&app).await;
    let (member, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    let request: i64 = sqlx::query_scalar(
        "INSERT INTO requests (tmdb_id, media_type, imdb_id, title, year, status,
                               requested_by, created_at, status_changed_at)
         VALUES (1, 'movie', 'tt0000001', 'Dune', 2021, 'pending', ?, 1, 1) RETURNING id",
    )
    .bind(member)
    .fetch_one(&app.pool)
    .await
    .unwrap();

    let mut headers = json_post_headers();
    headers.push(("Cookie", admin.as_str()));
    let enable = app
        .request(
            "PUT",
            "/api/admin/worker",
            &headers,
            Some(r#"{"enabled":true}"#),
        )
        .await;
    assert_eq!(enable.status, 200);
    let create = app
        .request(
            "POST",
            &format!("/api/admin/requests/{request}/fetch"),
            &headers,
            Some("{}"),
        )
        .await;
    assert_eq!(create.status, 201, "{}", create.body);

    let token = TEST_INTERNAL_TOKEN;
    // Routine: settings, progress and a claim with nothing to hand out. The
    // claim that takes the job is not.
    assert_eq!(
        worker(&app, "GET", "/internal/settings", token, None)
            .await
            .status,
        200
    );
    let claim = worker(&app, "POST", "/internal/jobs/claim", token, None).await;
    assert_eq!(claim.status, 200, "{}", claim.body);
    let claim: Value = serde_json::from_str(&claim.body).unwrap();
    let job = claim["id"].as_i64().unwrap();
    let claim_id = claim["claim_id"].as_str().unwrap().to_string();
    let progress = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{job}/progress"),
        token,
        Some(&json!({"claim_id": claim_id, "rd_torrent_id": "SECRETRD1"})),
    )
    .await;
    assert_eq!(progress.status, 200, "{}", progress.body);
    assert_eq!(
        worker(&app, "POST", "/internal/jobs/claim", token, None)
            .await
            .status,
        204
    );
    // Not routine: a wrong token, and a report.
    let wrong = worker(&app, "GET", "/internal/settings", "not-the-token", None).await;
    assert_eq!(wrong.status, 401);
    let report = worker(
        &app,
        "POST",
        &format!("/internal/jobs/{job}/report"),
        token,
        Some(&json!({
            "claim_id": claim_id,
            "state": "needs_pick",
            "reason": "dry_run",
            "candidates": [{"title": "SECRET-CANDIDATE-TITLE", "score": 1}],
            "error": "SECRET-ERROR-TEXT",
        })),
    )
    .await;
    assert_eq!(report.status, 200, "{}", report.body);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let text = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    let internal: Vec<Value> = text
        .lines()
        .filter(|l| l.contains(r#""target":"mediaqueue::request""#))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            v["fields"]["path"]
                .as_str()
                .is_some_and(|p| p.starts_with("/internal/"))
        })
        .collect();
    let logged: Vec<(String, String, u64)> = internal
        .iter()
        .map(|v| {
            (
                v["fields"]["method"].as_str().unwrap().to_string(),
                v["fields"]["path"].as_str().unwrap().to_string(),
                v["fields"]["status"].as_u64().unwrap(),
            )
        })
        .collect();
    let report_path = format!("/internal/jobs/{job}/report");
    assert_eq!(
        logged,
        vec![
            ("POST".to_string(), "/internal/jobs/claim".to_string(), 200),
            ("GET".to_string(), "/internal/settings".to_string(), 401),
            ("POST".to_string(), report_path, 200),
        ],
        "only the claim that handed out a job, the failure and the report are logged:\n{text}"
    );

    for secret in [
        TEST_INTERNAL_TOKEN,
        "not-the-token",
        claim_id.as_str(),
        "SECRETRD1",
        "SECRET-CANDIDATE-TITLE",
        "SECRET-ERROR-TEXT",
        "secret note",
    ] {
        assert!(!text.contains(secret), "log contains {secret:?}:\n{text}");
    }
}
