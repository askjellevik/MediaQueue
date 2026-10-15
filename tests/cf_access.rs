//! Cloudflare Access check Only built with the feature:
//! `cargo test --features cf-access --test cf_access`.
#![cfg(feature = "cf-access")]

mod common;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{
    HttpResponse, TEST_ADMIN_PASSWORD, TEST_ADMIN_USER, TestApp, cookie_pair, json_post_headers,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use mediaqueue::config::CfAccessConfig;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const ISSUER: &str = "https://team.cloudflareaccess.com";
const AUD: &str = "test-aud-tag";
/// A throwaway key made for these tests (tests/fixtures), never used
/// anywhere else.
const PRIVATE_KEY_DER: &[u8] = include_bytes!("fixtures/cf_access_test_key.der");
const MODULUS: &str = include_str!("fixtures/cf_access_test_key.n");

async fn spawn_with_access(certs: &MockServer) -> TestApp {
    let certs_url = format!("{}/cdn-cgi/access/certs", certs.uri());
    common::spawn_app_with(move |cfg| {
        cfg.cf_access = Some(CfAccessConfig {
            issuer: ISSUER.into(),
            audience: AUD.into(),
            certs_url,
        });
    })
    .await
}

async fn mount_certs(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/cdn-cgi/access/certs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "keys": [{
                "kid": "key-1", "kty": "RSA", "alg": "RS256", "use": "sig",
                "e": "AQAB", "n": MODULUS.trim()
            }],
            "public_cert": { "kid": "key-1", "cert": "unused" }
        })))
        .mount(server)
        .await;
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn token(kid: &str, aud: &str, iss: &str, exp: u64) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.into());
    let claims = json!({
        "aud": [aud], "iss": iss, "exp": exp, "iat": now(),
        "email": "admin@example.com", "sub": "user-id", "type": "app"
    });
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_der(PRIVATE_KEY_DER),
    )
    .unwrap()
}

fn good_token() -> String {
    token("key-1", AUD, ISSUER, now() + 300)
}

async fn get(app: &TestApp, path: &str, headers: &[(&str, &str)]) -> HttpResponse {
    app.request("GET", path, headers, None).await
}

async fn certs_fetches(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

#[tokio::test]
async fn admin_area_needs_a_valid_assertion() {
    let certs = MockServer::start().await;
    mount_certs(&certs).await;
    let app = spawn_with_access(&certs).await;
    let good = good_token();

    // Without an assertion the admin page and API are closed, even login.
    for path in ["/admin", "/admin.html", "/api/admin/requests"] {
        assert_eq!(get(&app, path, &[]).await.status, 403, "{path}");
    }
    let body = format!(r#"{{"username":"{TEST_ADMIN_USER}","password":"{TEST_ADMIN_PASSWORD}"}}"#);
    let blocked = app
        .request(
            "POST",
            "/api/admin/login",
            &json_post_headers(),
            Some(&body),
        )
        .await;
    assert_eq!(blocked.status, 403);

    // With one, the request reaches the app's own admin login.
    assert_eq!(
        get(&app, "/admin", &[("Cf-Access-Jwt-Assertion", &good)])
            .await
            .status,
        200
    );
    let mut headers = json_post_headers();
    headers.push(("Cf-Access-Jwt-Assertion", &good));
    let login = app
        .request("POST", "/api/admin/login", &headers, Some(&body))
        .await;
    assert_eq!(login.status, 200, "{}", login.body);
    let admin = cookie_pair(login.header("set-cookie").unwrap()).to_string();
    let listed = get(
        &app,
        "/api/admin/requests",
        &[("Cf-Access-Jwt-Assertion", &good), ("Cookie", &admin)],
    )
    .await;
    assert_eq!(listed.status, 200);
    // The admin cookie alone is not enough.
    assert_eq!(
        get(&app, "/api/admin/requests", &[("Cookie", &admin)])
            .await
            .status,
        403
    );
    // And the assertion alone is not enough either.
    let no_cookie = get(
        &app,
        "/api/admin/requests",
        &[("Cf-Access-Jwt-Assertion", &good)],
    )
    .await;
    assert_eq!(no_cookie.status, 401);

    // Keys were fetched once and cached.
    assert_eq!(certs_fetches(&certs).await, 1);
}

#[tokio::test]
async fn bad_assertions_are_refused() {
    let certs = MockServer::start().await;
    mount_certs(&certs).await;
    let app = spawn_with_access(&certs).await;

    let mut forged = good_token();
    forged.pop();
    forged.push(if forged.ends_with('A') { 'B' } else { 'A' });
    for bad in [
        token("key-1", "other-aud", ISSUER, now() + 300),
        token(
            "key-1",
            AUD,
            "https://evil.cloudflareaccess.com",
            now() + 300,
        ),
        token("key-1", AUD, ISSUER, now() - 3600),
        token("unknown-key", AUD, ISSUER, now() + 300),
        forged,
        "not-a-jwt".to_string(),
    ] {
        let response = get(&app, "/admin", &[("Cf-Access-Jwt-Assertion", &bad)]).await;
        assert_eq!(response.status, 403, "{bad}");
    }
}

#[tokio::test]
async fn unknown_key_ids_refetch_at_most_once_a_minute() {
    let certs = MockServer::start().await;
    mount_certs(&certs).await;
    let app = spawn_with_access(&certs).await;

    let unknown = token("rotated-key", AUD, ISSUER, now() + 300);
    for _ in 0..5 {
        assert_eq!(
            get(&app, "/admin", &[("Cf-Access-Jwt-Assertion", &unknown)])
                .await
                .status,
            403
        );
    }
    // One initial fetch plus one forced refresh, not one per request.
    assert_eq!(certs_fetches(&certs).await, 2);

    app.clock.advance(Duration::from_secs(61));
    assert_eq!(
        get(&app, "/admin", &[("Cf-Access-Jwt-Assertion", &unknown)])
            .await
            .status,
        403
    );
    assert_eq!(certs_fetches(&certs).await, 3);
}

#[tokio::test]
async fn member_side_is_untouched_and_keys_endpoint_down_fails_closed() {
    let certs = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&certs)
        .await;
    let app = spawn_with_access(&certs).await;

    assert_eq!(get(&app, "/", &[]).await.status, 200);
    assert_eq!(
        get(&app, "/api/me", &[]).await.status,
        401,
        "normal auth, no 403"
    );
    assert_eq!(get(&app, "/healthz", &[]).await.status, 200);
    let good = good_token();
    assert_eq!(
        get(&app, "/admin", &[("Cf-Access-Jwt-Assertion", &good)])
            .await
            .status,
        403
    );
}
