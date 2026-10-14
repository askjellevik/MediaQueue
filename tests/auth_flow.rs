//! Phase 1 acceptance tests: access codes, sessions, cookies, CSRF guard
//! and admin login, against the real app on a random port.

mod common;

use common::{
    HttpResponse, TestApp, cookie_pair, create_member, create_member_with_sessions,
    json_post_headers,
};

fn assert_json_error(resp: &HttpResponse, status: u16) {
    assert_eq!(resp.status, status, "body was: {}", resp.body);
    assert!(resp.body.contains("\"error\""), "body was: {}", resp.body);
    assert!(resp.body.contains("\"message\""), "body was: {}", resp.body);
}

async fn post_json(app: &TestApp, path: &str, body: &str) -> HttpResponse {
    app.request("POST", path, &json_post_headers(), Some(body))
        .await
}

async fn login_with_code(app: &TestApp, code: &str) -> HttpResponse {
    let body = format!(r#"{{"code":"{code}"}}"#);
    post_json(app, "/api/auth/code", &body).await
}

// ---------------------------------------------------------------------
// Cookie attributes
// ---------------------------------------------------------------------

#[tokio::test]
async fn login_sets_host_prefixed_secure_cookie() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;

    let resp = login_with_code(&app, &code).await;
    assert_eq!(resp.status, 200, "body was: {}", resp.body);

    let set_cookie = resp
        .header("set-cookie")
        .expect("Set-Cookie header present");
    assert!(set_cookie.starts_with("__Host-mp_user="));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("Secure"));
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Path=/"));
    assert!(!set_cookie.contains("Domain="));
}

#[tokio::test]
async fn dev_insecure_cookie_drops_prefix_and_secure() {
    let app = common::spawn_app_with(|cfg| {
        cfg.cookie_dev_insecure = true;
        cfg.public_origin = "http://localhost:3000".to_string();
    })
    .await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;

    let mut headers = json_post_headers();
    headers[0] = ("Origin", "http://localhost:3000");
    let body = format!(r#"{{"code":"{code}"}}"#);
    let resp = app
        .request("POST", "/api/auth/code", &headers, Some(&body))
        .await;
    assert_eq!(resp.status, 200, "body was: {}", resp.body);

    let set_cookie = resp
        .header("set-cookie")
        .expect("Set-Cookie header present");
    assert!(set_cookie.starts_with("mp_user="));
    assert!(!set_cookie.contains("__Host-"));
    assert!(!set_cookie.contains("Secure"));
}

// ---------------------------------------------------------------------
// Code normalisation and uniform failure response
// ---------------------------------------------------------------------

#[tokio::test]
async fn code_normalisation_variants_all_authenticate() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;

    let lower = code.to_lowercase();
    let with_dash = media_requests::auth::format_code_for_display(&code);
    let with_spaces = with_dash.replace('-', " ");

    for variant in [
        code.as_str(),
        lower.as_str(),
        with_dash.as_str(),
        with_spaces.as_str(),
    ] {
        let resp = login_with_code(&app, variant).await;
        assert_eq!(
            resp.status, 200,
            "variant {variant:?} body was: {}",
            resp.body
        );
        assert!(resp.body.contains("\"name\":\"Alice\""));
    }
}

#[tokio::test]
async fn wrong_rotated_and_revoked_codes_return_identical_response() {
    let app = common::spawn_app().await;
    let (member_id, code) = create_member(&app.pool, &app.config, "Alice").await;

    // Wrong: a code that was never issued.
    let wrong = login_with_code(&app, "ZZZZZZZZ").await;
    assert_json_error(&wrong, 401);

    // Rotated: replace the stored hash so the original code no longer matches.
    let new_hash = media_requests::auth::hmac_code(&app.config.code_pepper, "ROTATEDXX");
    sqlx::query("UPDATE members SET code_hash = ? WHERE id = ?")
        .bind(&new_hash)
        .bind(member_id)
        .execute(&app.pool)
        .await
        .expect("rotate member code");
    let rotated = login_with_code(&app, &code).await;
    assert_json_error(&rotated, 401);

    // Revoked: a second member, revoked, whose original code still hashes
    // to the stored value.
    let (member2_id, code2) = create_member(&app.pool, &app.config, "Bob").await;
    sqlx::query("UPDATE members SET revoked_at = ? WHERE id = ?")
        .bind(media_requests::db::now_unix())
        .bind(member2_id)
        .execute(&app.pool)
        .await
        .expect("revoke member");
    let revoked = login_with_code(&app, &code2).await;
    assert_json_error(&revoked, 401);

    assert_eq!(wrong.status, rotated.status);
    assert_eq!(rotated.status, revoked.status);
    assert_eq!(wrong.body, rotated.body);
    assert_eq!(rotated.body, revoked.body);
}

// ---------------------------------------------------------------------
// max_sessions eviction
// ---------------------------------------------------------------------

#[tokio::test]
async fn fourth_login_evicts_oldest_session_for_max_sessions_three() {
    let app = common::spawn_app().await;
    let (_, code) = create_member_with_sessions(&app.pool, &app.config, "Alice", 3).await;

    let first = login_with_code(&app, &code).await;
    assert_eq!(first.status, 200);
    let first_cookie = cookie_pair(first.header("set-cookie").unwrap()).to_string();

    for _ in 0..2 {
        let resp = login_with_code(&app, &code).await;
        assert_eq!(resp.status, 200);
    }

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE kind = 'user'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 3);

    // Fourth login evicts the oldest (the first device's) session.
    let fourth = login_with_code(&app, &code).await;
    assert_eq!(fourth.status, 200);

    let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE kind = 'user'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        count_after, 3,
        "total session count must stay at max_sessions"
    );

    // The first device's session cookie must now be rejected.
    let me = app
        .request("GET", "/api/me", &[("Cookie", &first_cookie)], None)
        .await;
    assert_eq!(me.status, 401, "first device should have been logged out");
}

// ---------------------------------------------------------------------
// CSRF / Origin / content-type guard
// ---------------------------------------------------------------------

#[tokio::test]
async fn missing_x_requested_with_returns_403() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let body = format!(r#"{{"code":"{code}"}}"#);
    let headers = [
        ("Origin", "https://example.com"),
        ("Content-Type", "application/json"),
    ];
    let resp = app
        .request("POST", "/api/auth/code", &headers, Some(&body))
        .await;
    assert_json_error(&resp, 403);
}

#[tokio::test]
async fn wrong_origin_returns_403() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let body = format!(r#"{{"code":"{code}"}}"#);
    let headers = [
        ("Origin", "https://evil.example"),
        ("X-Requested-With", "mp"),
        ("Content-Type", "application/json"),
    ];
    let resp = app
        .request("POST", "/api/auth/code", &headers, Some(&body))
        .await;
    assert_json_error(&resp, 403);
}

#[tokio::test]
async fn non_json_content_type_returns_403() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let body = format!(r#"{{"code":"{code}"}}"#);
    let headers = [
        ("Origin", "https://example.com"),
        ("X-Requested-With", "mp"),
        ("Content-Type", "text/plain"),
    ];
    let resp = app
        .request("POST", "/api/auth/code", &headers, Some(&body))
        .await;
    assert_json_error(&resp, 403);
}

#[tokio::test]
async fn get_requests_are_not_subject_to_the_csrf_guard() {
    let app = common::spawn_app().await;
    // No Origin/X-Requested-With at all; GET /api/me should still 401
    // cleanly on the missing session, not 403 on a CSRF check.
    let resp = app.request("GET", "/api/me", &[], None).await;
    assert_eq!(resp.status, 401);
}

// ---------------------------------------------------------------------
// GET /api/me and logout
// ---------------------------------------------------------------------

#[tokio::test]
async fn me_requires_session_and_reports_remaining_today() {
    let app = common::spawn_app().await;
    let resp = app.request("GET", "/api/me", &[], None).await;
    assert_eq!(resp.status, 401);

    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let login = login_with_code(&app, &code).await;
    let cookie = cookie_pair(login.header("set-cookie").unwrap()).to_string();

    let me = app
        .request("GET", "/api/me", &[("Cookie", &cookie)], None)
        .await;
    assert_eq!(me.status, 200);
    assert!(me.body.contains("\"name\":\"Alice\""));
    assert!(me.body.contains("\"remaining_today\":10"));
}

#[tokio::test]
async fn logout_clears_cookie_and_invalidates_session() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let login = login_with_code(&app, &code).await;
    let cookie = cookie_pair(login.header("set-cookie").unwrap()).to_string();

    let logout = app
        .request(
            "POST",
            "/api/auth/logout",
            &[
                ("Origin", "https://example.com"),
                ("X-Requested-With", "mp"),
                ("Content-Type", "application/json"),
                ("Cookie", &cookie),
            ],
            Some("{}"),
        )
        .await;
    assert_eq!(logout.status, 200, "body was: {}", logout.body);
    let cleared = logout.header("set-cookie").expect("clears cookie");
    assert!(cleared.contains("Max-Age=0"));

    let me = app
        .request("GET", "/api/me", &[("Cookie", &cookie)], None)
        .await;
    assert_eq!(me.status, 401, "session must be gone after logout");
}

// ---------------------------------------------------------------------
// Database never holds a raw code or token
// ---------------------------------------------------------------------

#[tokio::test]
async fn database_holds_no_raw_code_or_token() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let login = login_with_code(&app, &code).await;
    let cookie_value = cookie_pair(login.header("set-cookie").unwrap())
        .split('=')
        .nth(1)
        .unwrap()
        .to_string();

    let stored_code_hash: Vec<u8> =
        sqlx::query_scalar("SELECT code_hash FROM members WHERE name = 'Alice'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_ne!(stored_code_hash, code.as_bytes());
    assert_ne!(
        stored_code_hash,
        media_requests::auth::normalise_code(&code).as_bytes()
    );

    let stored_token_hash: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM sessions WHERE kind = 'user'")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_ne!(stored_token_hash, cookie_value.as_bytes());
}

// ---------------------------------------------------------------------
// Admin login/logout
// ---------------------------------------------------------------------

#[tokio::test]
async fn admin_login_requires_require_admin_extractor_for_logout() {
    let app = common::spawn_app().await;
    let resp = app
        .request(
            "POST",
            "/api/admin/logout",
            &json_post_headers(),
            Some("{}"),
        )
        .await;
    assert_eq!(resp.status, 401);
}

#[tokio::test]
async fn admin_login_succeeds_and_sets_host_prefixed_cookie() {
    let app = common::spawn_app().await;
    let body = format!(
        r#"{{"username":"{}","password":"{}"}}"#,
        common::TEST_ADMIN_USER,
        common::TEST_ADMIN_PASSWORD
    );
    let resp = post_json(&app, "/api/admin/login", &body).await;
    assert_eq!(resp.status, 200, "body was: {}", resp.body);

    let set_cookie = resp
        .header("set-cookie")
        .expect("Set-Cookie header present");
    assert!(set_cookie.starts_with("__Host-mp_admin="));
    assert!(set_cookie.contains("Secure"));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
}

#[tokio::test]
async fn admin_login_rejects_unknown_username_and_wrong_password_identically() {
    let app = common::spawn_app().await;

    let unknown = post_json(
        &app,
        "/api/admin/login",
        r#"{"username":"nobody","password":"whatever"}"#,
    )
    .await;
    assert_json_error(&unknown, 401);

    let body = format!(
        r#"{{"username":"{}","password":"wrong-password"}}"#,
        common::TEST_ADMIN_USER
    );
    let wrong_password = post_json(&app, "/api/admin/login", &body).await;
    assert_json_error(&wrong_password, 401);

    assert_eq!(unknown.status, wrong_password.status);
    assert_eq!(unknown.body, wrong_password.body);
}

#[tokio::test]
async fn admin_logout_clears_cookie_and_invalidates_session() {
    let app = common::spawn_app().await;
    let body = format!(
        r#"{{"username":"{}","password":"{}"}}"#,
        common::TEST_ADMIN_USER,
        common::TEST_ADMIN_PASSWORD
    );
    let login = post_json(&app, "/api/admin/login", &body).await;
    let cookie = cookie_pair(login.header("set-cookie").unwrap()).to_string();

    let logout = app
        .request(
            "POST",
            "/api/admin/logout",
            &[
                ("Origin", "https://example.com"),
                ("X-Requested-With", "mp"),
                ("Content-Type", "application/json"),
                ("Cookie", &cookie),
            ],
            Some("{}"),
        )
        .await;
    assert_eq!(logout.status, 200, "body was: {}", logout.body);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE kind = 'admin'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
