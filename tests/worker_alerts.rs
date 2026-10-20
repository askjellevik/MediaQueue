//! The worker's alert endpoint and the push for an automatic add, against a
//! wiremock ntfy: the portal writes the text, one alert per kind per 30
//! minutes, and only jobs the auto sweep created announce themselves.

mod common;

use std::time::Duration;

use common::{HttpResponse, TEST_INTERNAL_TOKEN, TestApp, json_post_headers, login_admin_cookie};
use mediaqueue::config::NtfyConfig;
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const TOPIC: &str = "alert-test-topic";
const NTFY_TOKEN: &str = "ntfy-test-token";

async fn app_with_ntfy() -> (TestApp, MockServer) {
    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&ntfy)
        .await;
    let uri = ntfy.uri();
    let app = common::spawn_app_with(|cfg| {
        cfg.ntfy = NtfyConfig {
            url: Some(uri),
            topic: TOPIC.into(),
            token: Some(NTFY_TOKEN.into()),
        };
    })
    .await;
    (app, ntfy)
}

async fn worker(
    app: &TestApp,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> HttpResponse {
    let auth = token.map(|t| format!("Bearer {t}"));
    let body = body.map(Value::to_string);
    let mut headers = vec![("Content-Type", "application/json")];
    if let Some(auth) = &auth {
        headers.push(("Authorization", auth.as_str()));
    }
    app.internal_request("POST", path, &headers, body.as_deref())
        .await
}

async fn alert(app: &TestApp, kind: &str) -> HttpResponse {
    worker(
        app,
        "/internal/alert",
        Some(&json!({ "kind": kind })),
        Some(TEST_INTERNAL_TOKEN),
    )
    .await
}

/// The pushes that have reached ntfy so far, after giving the background
/// sender a moment.
async fn pushes(ntfy: &MockServer) -> Vec<Value> {
    tokio::time::sleep(Duration::from_millis(300)).await;
    ntfy.received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn each_kind_sends_one_high_priority_warning_with_portal_written_text() {
    let (app, ntfy) = app_with_ntfy().await;
    for kind in [
        "breaker_open",
        "rd_auth",
        "counters_unreadable",
        "repeated_failures",
    ] {
        let response = alert(&app, kind).await;
        assert_eq!(response.status, 200, "{kind}: {}", response.body);
        assert_eq!(response.body, r#"{"ok":true}"#);
    }
    let sent = pushes(&ntfy).await;
    assert_eq!(sent.len(), 4);
    let mut titles = Vec::new();
    for push in &sent {
        assert_eq!(push["topic"], TOPIC);
        assert_eq!(push["priority"], 4);
        assert_eq!(push["tags"], json!(["warning"]));
        assert!(!push["message"].as_str().unwrap().is_empty());
        titles.push(push["title"].as_str().unwrap().to_string());
    }
    titles.sort();
    titles.dedup();
    assert_eq!(titles.len(), 4, "every kind has its own text");
    for request in ntfy.received_requests().await.unwrap() {
        assert_eq!(
            request
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {NTFY_TOKEN}")
        );
    }
}

#[tokio::test]
async fn a_second_alert_of_the_same_kind_within_thirty_minutes_is_held_back() {
    let (app, ntfy) = app_with_ntfy().await;
    assert_eq!(alert(&app, "breaker_open").await.status, 200);
    app.clock.advance(Duration::from_secs(29 * 60));
    let again = alert(&app, "breaker_open").await;
    assert_eq!(again.status, 200);
    assert_eq!(again.body, r#"{"ok":true}"#);
    // Another kind is not held back by it.
    assert_eq!(alert(&app, "rd_auth").await.status, 200);
    assert_eq!(pushes(&ntfy).await.len(), 2);

    app.clock.advance(Duration::from_secs(2 * 60));
    assert_eq!(alert(&app, "breaker_open").await.status, 200);
    assert_eq!(pushes(&ntfy).await.len(), 3);
}

#[tokio::test]
async fn bad_bodies_and_missing_tokens_are_refused_and_send_nothing() {
    let (app, ntfy) = app_with_ntfy().await;
    for body in [
        json!({ "kind": "disk_full" }),
        json!({ "kind": "" }),
        json!({}),
        json!({ "kind": "rd_auth", "text": "hello" }),
    ] {
        let response = worker(
            &app,
            "/internal/alert",
            Some(&body),
            Some(TEST_INTERNAL_TOKEN),
        )
        .await;
        assert_eq!(response.status, 400, "{body}: {}", response.body);
    }
    let body = json!({ "kind": "rd_auth" });
    assert_eq!(
        worker(&app, "/internal/alert", Some(&body), None)
            .await
            .status,
        401
    );
    assert_eq!(
        worker(&app, "/internal/alert", Some(&body), Some("wrong-token"))
            .await
            .status,
        401
    );
    assert!(pushes(&ntfy).await.is_empty());
}

#[tokio::test]
async fn an_alert_without_ntfy_configured_is_still_ok() {
    let app = common::spawn_app().await;
    assert_eq!(alert(&app, "rd_auth").await.status, 200);
}

// ---------------------------------------------------------------------
// The push for an automatic add
// ---------------------------------------------------------------------

async fn admin_post(app: &TestApp, cookie: &str, path: &str) -> HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("Cookie", cookie));
    app.request("POST", path, &headers, Some("{}")).await
}

/// A claimed job for a pending request titled Dune; `origin` is set as the
/// sweep or the admin button would have.
async fn claimed_job(app: &TestApp, origin: &str) -> (i64, String, i64) {
    let admin = login_admin_cookie(app).await;
    let (member, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    let now = i64::try_from(app.clock.unix_secs()).unwrap();
    let request_id: i64 = sqlx::query_scalar(
        "INSERT INTO requests (tmdb_id, media_type, imdb_id, title, year, status, requested_by,
                               created_at, status_changed_at)
         VALUES (7, 'movie', 'tt0000007', 'Dune', 2021, 'pending', ?, ?, ?) RETURNING id",
    )
    .bind(member)
    .bind(now)
    .bind(now)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    let mut headers = json_post_headers();
    headers.push(("Cookie", &admin));
    let enabled = app
        .request(
            "PUT",
            "/api/admin/worker",
            &headers,
            Some(r#"{"enabled":true}"#),
        )
        .await;
    assert_eq!(enabled.status, 200, "{}", enabled.body);
    let created = admin_post(
        app,
        &admin,
        &format!("/api/admin/requests/{request_id}/fetch"),
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.body);
    sqlx::query("UPDATE fetch_jobs SET origin = ?")
        .bind(origin)
        .execute(&app.pool)
        .await
        .unwrap();
    let claim = worker(app, "/internal/jobs/claim", None, Some(TEST_INTERNAL_TOKEN)).await;
    assert_eq!(claim.status, 200, "{}", claim.body);
    let claim: Value = serde_json::from_str(&claim.body).unwrap();
    (
        claim["id"].as_i64().unwrap(),
        claim["claim_id"].as_str().unwrap().to_string(),
        request_id,
    )
}

async fn report(app: &TestApp, job: i64, claim_id: &str, body: Value) -> HttpResponse {
    let mut body = body;
    body["claim_id"] = json!(claim_id);
    worker(
        app,
        &format!("/internal/jobs/{job}/report"),
        Some(&body),
        Some(TEST_INTERNAL_TOKEN),
    )
    .await
}

#[tokio::test]
async fn a_done_report_from_an_auto_job_pushes_lagt_til_automatisk() {
    let (app, ntfy) = app_with_ntfy().await;
    let (job, claim_id, request_id) = claimed_job(&app, "auto").await;
    let response = report(
        &app,
        job,
        &claim_id,
        json!({"state": "done", "rd_torrent_id": "RD1"}),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);

    let sent = pushes(&ntfy).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["title"], "Lagt til automatisk: Dune");
    assert_eq!(sent[0]["topic"], TOPIC);
    let status: String = sqlx::query_scalar("SELECT status FROM requests WHERE id = ?")
        .bind(request_id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(status, "added");
}

#[tokio::test]
async fn no_push_for_an_admin_job_a_refused_report_or_something_that_was_already_there() {
    // An admin's job.
    let (app, ntfy) = app_with_ntfy().await;
    let (job, claim_id, _) = claimed_job(&app, "admin").await;
    let response = report(
        &app,
        job,
        &claim_id,
        json!({"state": "done", "rd_torrent_id": "RD1"}),
    )
    .await;
    assert_eq!(response.status, 200);
    assert!(pushes(&ntfy).await.is_empty());

    // An auto job whose report is refused (stale claim), then one that does
    // not finish.
    let (app, ntfy) = app_with_ntfy().await;
    let (job, claim_id, _) = claimed_job(&app, "auto").await;
    let stale = report(
        &app,
        job,
        "not-the-claim",
        json!({"state": "done", "rd_torrent_id": "RD1"}),
    )
    .await;
    assert_eq!(stale.status, 409);
    let missing = report(&app, job, &claim_id, json!({"state": "done"})).await;
    assert_eq!(missing.status, 400);
    let failed = report(
        &app,
        job,
        &claim_id,
        json!({"state": "failed", "reason": "rd_refused"}),
    )
    .await;
    assert_eq!(failed.status, 200);
    assert!(pushes(&ntfy).await.is_empty());

    // An auto job that finds the title already at Real-Debrid is not "added".
    let (app, ntfy) = app_with_ntfy().await;
    let (job, claim_id, _) = claimed_job(&app, "auto").await;
    let had = report(
        &app,
        job,
        &claim_id,
        json!({"state": "done", "reason": "already_had"}),
    )
    .await;
    assert_eq!(had.status, 200);
    assert!(pushes(&ntfy).await.is_empty());
}

#[tokio::test]
async fn no_push_when_the_request_was_closed_by_hand_meanwhile() {
    let (app, ntfy) = app_with_ntfy().await;
    let (job, claim_id, request_id) = claimed_job(&app, "auto").await;
    sqlx::query("UPDATE requests SET status = 'added' WHERE id = ?")
        .bind(request_id)
        .execute(&app.pool)
        .await
        .unwrap();
    let response = report(
        &app,
        job,
        &claim_id,
        json!({"state": "done", "rd_torrent_id": "RD1"}),
    )
    .await;
    // The job finishes, but the request did not change, so nothing is pushed.
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(pushes(&ntfy).await.is_empty());
}
