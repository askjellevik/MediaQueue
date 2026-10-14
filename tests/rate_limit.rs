//! Phase 2 acceptance tests: rate limits, lockouts and the global login
//! throttle, against the real app on a random port. Time comes from the
//! app's fake clock, so nothing here sleeps to wait out a limit.

mod common;

use std::{
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use common::{HttpResponse, TestApp, create_member, json_post_headers};

fn assert_429(resp: &HttpResponse, retry_after: u64) {
    assert_eq!(resp.status, 429, "body was: {}", resp.body);
    assert_eq!(
        resp.header("retry-after"),
        Some(retry_after.to_string().as_str()),
        "Retry-After"
    );
    assert_eq!(
        resp.body,
        r#"{"error":"rate_limited","message":"too many requests, try again later"}"#
    );
    assert_eq!(resp.header("cache-control"), Some("no-store"));
}

/// POSTs an access code as if Caddy forwarded it for `client_ip` (the test
/// client connects from loopback, which the test config trusts).
async fn post_code(app: &TestApp, client_ip: &str, code: &str) -> HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("X-Real-IP", client_ip));
    let body = format!(r#"{{"code":"{code}"}}"#);
    app.request("POST", "/api/auth/code", &headers, Some(&body))
        .await
}

async fn post_admin_login(app: &TestApp, client_ip: &str, password: &str) -> HttpResponse {
    let mut headers = json_post_headers();
    headers.push(("X-Real-IP", client_ip));
    let body = format!(
        r#"{{"username":"{}","password":"{password}"}}"#,
        common::TEST_ADMIN_USER
    );
    app.request("POST", "/api/admin/login", &headers, Some(&body))
        .await
}

async fn get_me(app: &TestApp, client_ip: &str) -> HttpResponse {
    app.request("GET", "/api/me", &[("X-Real-IP", client_ip)], None)
        .await
}

/// A distinct IPv4 address per `n`, for spreading failures across IPs.
fn nth_ip(n: u32) -> String {
    Ipv4Addr::from_bits(0xc633_6400 + n).to_string() // 198.51.100.0 + n
}

/// Records `count` wrong-code failures straight into the limiter, each
/// from its own IP, to build up global throttle pressure quickly.
fn inject_code_failures(app: &TestApp, from: u32, count: u32) {
    for n in from..from + count {
        let ip: IpAddr = Ipv4Addr::from_bits(0x0a00_0000 + n).into();
        app.state.limits.code_login.record_failure(ip);
    }
}

const WRONG_CODE: &str = "ZZZZ-ZZZZ";

// ---------------------------------------------------------------------
// Per-IP lockouts
// ---------------------------------------------------------------------

#[tokio::test]
async fn sixth_wrong_code_is_locked_out_with_growing_backoff() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let attacker = "203.0.113.50";

    for _ in 0..5 {
        assert_eq!(post_code(&app, attacker, WRONG_CODE).await.status, 401);
    }
    assert_429(&post_code(&app, attacker, WRONG_CODE).await, 900);
    // While locked out even the right code is refused from that IP...
    assert_429(&post_code(&app, attacker, &code).await, 900);
    // ...but another IP is unaffected, for right and wrong codes alike.
    assert_eq!(post_code(&app, "203.0.113.51", &code).await.status, 200);
    assert_eq!(
        post_code(&app, "203.0.113.51", WRONG_CODE).await.status,
        401
    );

    app.clock.advance(Duration::from_secs(600));
    assert_429(&post_code(&app, attacker, WRONG_CODE).await, 300);

    // Second lockout: one hour.
    app.clock.advance(Duration::from_secs(300));
    for _ in 0..5 {
        assert_eq!(post_code(&app, attacker, WRONG_CODE).await.status, 401);
    }
    assert_429(&post_code(&app, attacker, WRONG_CODE).await, 3600);

    // Third lockout: a day.
    app.clock.advance(Duration::from_secs(3600));
    for _ in 0..5 {
        assert_eq!(post_code(&app, attacker, WRONG_CODE).await.status, 401);
    }
    assert_429(&post_code(&app, attacker, WRONG_CODE).await, 86_400);

    // Once it expires the right code works again.
    app.clock.advance(Duration::from_secs(86_400));
    assert_eq!(post_code(&app, attacker, &code).await.status, 200);
}

#[tokio::test]
async fn ipv6_clients_in_one_64_share_a_lockout() {
    let app = common::spawn_app().await;
    for i in 1..=5 {
        let ip = format!("2001:db8:aa:bb::{i}");
        assert_eq!(post_code(&app, &ip, WRONG_CODE).await.status, 401);
    }
    assert_429(
        &post_code(&app, "2001:db8:aa:bb:ffff::1", WRONG_CODE).await,
        900,
    );
    assert_eq!(
        post_code(&app, "2001:db8:aa:bc::1", WRONG_CODE)
            .await
            .status,
        401
    );
}

#[tokio::test]
async fn admin_login_is_locked_out_per_ip_independently_of_codes() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    let attacker = "203.0.113.60";

    for _ in 0..5 {
        let resp = post_admin_login(&app, attacker, "wrong-password").await;
        assert_eq!(resp.status, 401);
    }
    assert_429(
        &post_admin_login(&app, attacker, "wrong-password").await,
        900,
    );
    assert_429(
        &post_admin_login(&app, attacker, common::TEST_ADMIN_PASSWORD).await,
        900,
    );
    // Code entry from the same IP has its own tracker.
    assert_eq!(post_code(&app, attacker, &code).await.status, 200);
    // And the admin can still log in from elsewhere.
    let other = post_admin_login(&app, "203.0.113.61", common::TEST_ADMIN_PASSWORD).await;
    assert_eq!(other.status, 200, "body was: {}", other.body);
}

// ---------------------------------------------------------------------
// Global throttle
// ---------------------------------------------------------------------

#[tokio::test]
async fn global_throttle_delays_attempts_but_correct_code_still_logs_in() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;

    // 60 failures in the hour: not yet above the threshold.
    inject_code_failures(&app, 0, 60);
    let before = app.clock.elapsed();
    assert_eq!(post_code(&app, &nth_ip(1), &code).await.status, 200);
    assert_eq!(app.clock.elapsed(), before, "no delay at 60 failures");

    // The 61st failure, over HTTP, turns the 1 s tier on.
    assert_eq!(post_code(&app, &nth_ip(2), WRONG_CODE).await.status, 401);
    let before = app.clock.elapsed();
    let resp = post_code(&app, &nth_ip(3), &code).await;
    assert_eq!(resp.status, 200, "a correct code still logs in");
    assert_eq!(app.clock.elapsed() - before, Duration::from_secs(1));

    // Past 300 failures the delay is 3 s.
    inject_code_failures(&app, 100, 240);
    let before = app.clock.elapsed();
    assert_eq!(post_code(&app, &nth_ip(4), &code).await.status, 200);
    assert_eq!(app.clock.elapsed() - before, Duration::from_secs(3));
    // Wrong codes are delayed just the same.
    let before = app.clock.elapsed();
    assert_eq!(post_code(&app, &nth_ip(5), WRONG_CODE).await.status, 401);
    assert_eq!(app.clock.elapsed() - before, Duration::from_secs(3));

    // An hour after the failures stopped, attempts are fast again.
    app.clock.advance(Duration::from_secs(3600));
    let before = app.clock.elapsed();
    assert_eq!(post_code(&app, &nth_ip(6), &code).await.status, 200);
    assert_eq!(app.clock.elapsed(), before);
}

#[tokio::test]
async fn full_or_closed_delay_semaphore_returns_429_never_a_bypass() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    inject_code_failures(&app, 0, 61);

    let permits = app.state.limits.code_login.throttle.permits().clone();
    let held = permits
        .clone()
        .acquire_many_owned(32)
        .await
        .expect("take every delay permit");
    assert_429(&post_code(&app, &nth_ip(1), &code).await, 1);
    drop(held);
    assert_eq!(post_code(&app, &nth_ip(1), &code).await.status, 200);

    // An internal failure (here a closed semaphore) is still a 429.
    permits.close();
    assert_429(&post_code(&app, &nth_ip(2), &code).await, 1);
}

#[tokio::test]
async fn admin_throttle_is_separate_from_code_throttle() {
    let app = common::spawn_app().await;
    let (_, code) = create_member(&app.pool, &app.config, "Alice").await;
    for n in 0..31 {
        let ip: IpAddr = Ipv4Addr::from_bits(0x0a00_0000 + n).into();
        app.state.limits.admin_login.record_failure(ip);
    }
    // Code entry is not slowed by admin-login failures...
    let before = app.clock.elapsed();
    assert_eq!(post_code(&app, &nth_ip(1), &code).await.status, 200);
    assert_eq!(app.clock.elapsed(), before);
    // ...while admin login is, above 30 failures per 15 minutes.
    let before = app.clock.elapsed();
    let resp = post_admin_login(&app, &nth_ip(2), common::TEST_ADMIN_PASSWORD).await;
    assert_eq!(resp.status, 200, "body was: {}", resp.body);
    assert_eq!(app.clock.elapsed() - before, Duration::from_secs(1));
}

// ---------------------------------------------------------------------
// Global per-IP limit on every /api call
// ---------------------------------------------------------------------

#[tokio::test]
async fn api_limit_bursts_refills_and_sets_retry_after() {
    let app = common::spawn_app_with(|cfg| cfg.rate_limits.api_per_minute = 3).await;
    let client = "203.0.113.70";

    for _ in 0..3 {
        assert_eq!(get_me(&app, client).await.status, 401);
    }
    // 3 per minute refills one call every 20 s.
    assert_429(&get_me(&app, client).await, 20);
    assert_eq!(get_me(&app, "203.0.113.71").await.status, 401);

    app.clock.advance(Duration::from_secs(15));
    assert_429(&get_me(&app, client).await, 5);
    app.clock.advance(Duration::from_secs(5));
    assert_eq!(get_me(&app, client).await.status, 401);
    assert_429(&get_me(&app, client).await, 20);
}

#[tokio::test]
async fn api_limit_runs_before_csrf_and_body_parsing() {
    let app = common::spawn_app_with(|cfg| cfg.rate_limits.api_per_minute = 1).await;
    let headers = [("X-Real-IP", "203.0.113.80")];
    // First call spends the budget (and fails CSRF: no headers).
    let first = app
        .request("POST", "/api/auth/code", &headers, Some("not json"))
        .await;
    assert_eq!(first.status, 403);
    // The second is refused before CSRF or JSON parsing even run.
    let second = app
        .request("POST", "/api/auth/code", &headers, Some("not json"))
        .await;
    assert_429(&second, 60);
}

#[tokio::test]
async fn ipv6_clients_in_one_64_share_an_api_bucket() {
    let app = common::spawn_app_with(|cfg| cfg.rate_limits.api_per_minute = 3).await;
    for ip in ["2001:db8:1:2::1", "2001:db8:1:2::2", "2001:db8:1:2:ab::3"] {
        assert_eq!(get_me(&app, ip).await.status, 401);
    }
    assert_429(&get_me(&app, "2001:db8:1:2:ffff:ffff:ffff:ffff").await, 20);
    assert_eq!(get_me(&app, "2001:db8:1:3::1").await.status, 401);
}

#[tokio::test]
async fn spoofed_x_real_ip_from_untrusted_peer_cannot_dodge_the_limit() {
    let app = common::spawn_app_with(|cfg| {
        cfg.rate_limits.api_per_minute = 3;
        // Loopback (the test client) is not a trusted proxy here.
        cfg.trusted_proxy_cidrs = media_requests::clientip::parse_cidr_list("10.0.0.0/8").unwrap();
    })
    .await;
    for n in 0..3 {
        assert_eq!(get_me(&app, &nth_ip(n)).await.status, 401);
    }
    assert_429(&get_me(&app, &nth_ip(99)).await, 20);
}

#[tokio::test]
async fn healthz_is_not_rate_limited() {
    let app = common::spawn_app_with(|cfg| cfg.rate_limits.api_per_minute = 1).await;
    for _ in 0..5 {
        let resp = app.request("GET", "/healthz", &[], None).await;
        assert_eq!(resp.status, 200);
    }
}

#[tokio::test]
async fn maintenance_evicts_idle_api_keys() {
    let app = common::spawn_app().await;
    for n in 0..20 {
        get_me(&app, &nth_ip(n)).await;
    }
    assert_eq!(app.state.limits.api_keys(), 20);
    app.clock.advance(Duration::from_secs(120));
    app.state.limits.maintain();
    assert_eq!(app.state.limits.api_keys(), 0);
}
