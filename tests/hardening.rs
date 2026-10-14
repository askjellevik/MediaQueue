//! Phase 7 review gaps: exact security headers everywhere, and the admin
//! session's idle and absolute limits.

mod common;

use common::{TEST_ADMIN_PASSWORD, TEST_ADMIN_USER, TestApp, cookie_pair, json_post_headers};

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' https://image.tmdb.org; connect-src 'self'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

#[tokio::test]
async fn every_response_carries_the_security_headers() {
    let app = common::spawn_app().await;
    for (method, path) in [
        ("GET", "/"),
        ("GET", "/admin"),
        ("GET", "/healthz"),
        ("GET", "/api/me"),
        ("GET", "/no-such-page"),
        ("POST", "/api/auth/code"),
    ] {
        let response = app.request(method, path, &[], None).await;
        let expect = [
            ("content-security-policy", CSP),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            (
                "permissions-policy",
                "camera=(), microphone=(), geolocation=()",
            ),
            ("cross-origin-opener-policy", "same-origin"),
        ];
        for (name, value) in expect {
            assert_eq!(
                response.header(name),
                Some(value),
                "{name} on {method} {path}"
            );
        }
        if path.starts_with("/api") || path == "/healthz" {
            assert_eq!(response.header("cache-control"), Some("no-store"), "{path}");
        }
    }
}

async fn admin_cookie(app: &TestApp) -> String {
    let body = format!(r#"{{"username":"{TEST_ADMIN_USER}","password":"{TEST_ADMIN_PASSWORD}"}}"#);
    let login = app
        .request(
            "POST",
            "/api/admin/login",
            &json_post_headers(),
            Some(&body),
        )
        .await;
    assert_eq!(login.status, 200);
    cookie_pair(login.header("set-cookie").unwrap()).to_string()
}

async fn admin_status(app: &TestApp, cookie: &str) -> u16 {
    app.request("GET", "/api/admin/members", &[("Cookie", cookie)], None)
        .await
        .status
}

#[tokio::test]
async fn admin_session_ends_after_two_idle_hours() {
    let app = common::spawn_app().await;
    let cookie = admin_cookie(&app).await;
    assert_eq!(admin_status(&app, &cookie).await, 200);

    // Last seen just under two hours ago: still fine (and touched).
    sqlx::query("UPDATE sessions SET last_seen_at = last_seen_at - 7000 WHERE kind = 'admin'")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(admin_status(&app, &cookie).await, 200);

    sqlx::query("UPDATE sessions SET last_seen_at = last_seen_at - 7201 WHERE kind = 'admin'")
        .execute(&app.pool)
        .await
        .unwrap();
    assert_eq!(admin_status(&app, &cookie).await, 401);
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE kind = 'admin'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "the expired session is deleted");
}

#[tokio::test]
async fn admin_session_ends_after_eight_hours_even_when_active() {
    let app = common::spawn_app().await;
    let cookie = admin_cookie(&app).await;
    // Active a minute ago, but created more than eight hours ago.
    sqlx::query(
        "UPDATE sessions SET created_at = created_at - 28801, expires_at = expires_at - 28801,
                             last_seen_at = last_seen_at - 60
         WHERE kind = 'admin'",
    )
    .execute(&app.pool)
    .await
    .unwrap();
    assert_eq!(admin_status(&app, &cookie).await, 401);
}

#[tokio::test]
async fn every_admin_login_issues_a_new_token() {
    let app = common::spawn_app().await;
    let first = admin_cookie(&app).await;
    let second = admin_cookie(&app).await;
    assert_ne!(first, second);
    assert_eq!(admin_status(&app, &first).await, 200);
    assert_eq!(admin_status(&app, &second).await, 200);
}
