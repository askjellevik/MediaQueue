//! Real-browser checks with headless Chrome. Ignored by default because
//! they need a local Chrome; run them with
//!
//! ```text
//! CHROME=/path/to/chrome cargo test --test browser -- --ignored
//! ```
//!
//! `CHROME` defaults to Chrome's usual path on Windows (reachable from WSL).
//! Console output is read from Chrome's log, so a CSP violation, a failed
//! module load or an uncaught exception fails the test.

mod common;

use std::{path::PathBuf, process::Command};

use mediaqueue::routes::static_files::asset_version;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, query_param},
};

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' https://image.tmdb.org; connect-src 'self'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

fn chrome() -> PathBuf {
    std::env::var_os("CHROME")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/mnt/c/Program Files/Google/Chrome/Application/chrome.exe".into())
}

/// Loads `url` in headless Chrome, lets it run, and returns the final DOM
/// and Chrome's log (which carries console messages).
/// Two headless Chromes sharing the default profile trip over each other.
static CHROME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn dump_dom(url: String) -> (String, String) {
    let _one_at_a_time = CHROME_LOCK.lock().await;
    tokio::task::spawn_blocking(move || {
        let output = Command::new(chrome())
            .args([
                "--headless=new",
                "--disable-gpu",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--enable-logging=stderr",
                "--v=0",
                "--virtual-time-budget=15000",
                "--dump-dom",
                &url,
            ])
            .output()
            .expect("run chrome; set CHROME to its path");
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    })
    .await
    .expect("chrome task")
}

/// Console errors, CSP refusals and uncaught exceptions from Chrome's log.
fn console_problems(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| {
            (l.contains("CONSOLE") && (l.contains(":ERROR:") || l.contains("Uncaught")))
                || l.contains("Refused to")
                || l.contains("Content Security Policy")
        })
        .collect()
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn gate_renders_under_the_real_csp_without_console_errors() {
    let app = common::spawn_app().await;
    let (dom, log) = dump_dom(format!("http://127.0.0.1:{}/", app.addr.port())).await;

    assert!(
        dom.contains(r#"class="gate""#),
        "gate not rendered:\n{dom}\n{log}"
    );
    assert!(dom.contains(r#"id="code""#));
    assert!(dom.contains(r#"autocomplete="one-time-code""#));
    assert!(dom.contains(r#"autocapitalize="characters""#));
    assert!(!dom.contains("combobox"), "search UI before login");
    assert!(
        console_problems(&log).is_empty(),
        "console: {:#?}",
        console_problems(&log)
    );
}

fn web_file(rel: &str) -> Vec<u8> {
    std::fs::read(format!("{}/{rel}", env!("CARGO_MANIFEST_DIR"))).expect("read web file")
}

async fn serve_file(server: &MockServer, url_path: &str, body: Vec<u8>, content_type: &str) {
    Mock::given(method("GET"))
        .and(path(url_path))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-security-policy", CSP)
                .insert_header("x-content-type-options", "nosniff")
                .set_body_raw(body, content_type),
        )
        .mount(server)
        .await;
}

async fn api_json(
    server: &MockServer,
    verb: &str,
    url_path: &str,
    status: u16,
    body: serde_json::Value,
) {
    Mock::given(method(verb))
        .and(path(url_path))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

/// An event stream that opens and says nothing, so the page's live-update
/// connection works without errors in the console.
async fn quiet_event_stream(server: &MockServer, url_path: &str) {
    Mock::given(method("GET"))
        .and(path(url_path))
        .respond_with(ResponseTemplate::new(200).set_body_raw(": ok\n\n", "text/event-stream"))
        .mount(server)
        .await;
}

fn result(title: &str, id: u32, state: &str) -> serde_json::Value {
    json!({
        "tmdb_id": id, "type": "movie", "title": title, "year": 2021,
        "poster": null, "overview": "Spice.",
        "states": { "plain": state, "pro": "none" }
    })
}

/// The real page and modules, served from wiremock with the real CSP and
/// a mocked API, plus `tests/js/driver.js`, which types, presses keys and
/// checks what renders.
#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn typing_race_and_keyboard_flow_work_end_to_end() {
    let server = MockServer::start().await;

    let index = String::from_utf8(web_file("web/index.html"))
        .unwrap()
        .replace("__V__", "t")
        .replace(
            "</head>",
            "<script type=\"module\" src=\"/v/t/driver.js\"></script>\n</head>",
        );
    serve_file(&server, "/", index.into_bytes(), "text/html; charset=utf-8").await;
    for (rel, content_type) in [
        ("js/search.js", "text/javascript"),
        ("js/api.js", "text/javascript"),
        ("js/live.js", "text/javascript"),
        ("js/ui.js", "text/javascript"),
        ("js/strings.js", "text/javascript"),
        ("js/theme.js", "text/javascript"),
        ("css/app.css", "text/css"),
        ("img/tmdb.svg", "image/svg+xml"),
        ("img/icon.svg", "image/svg+xml"),
    ] {
        serve_file(
            &server,
            &format!("/v/t/{rel}"),
            web_file(&format!("web/{rel}")),
            content_type,
        )
        .await;
    }
    serve_file(
        &server,
        "/v/t/driver.js",
        web_file("tests/js/driver.js"),
        "text/javascript",
    )
    .await;

    api_json(
        &server,
        "GET",
        "/api/me",
        200,
        json!({"name": "Alice", "remaining_today": 10}),
    )
    .await;
    // The first (slow) query must never render; the second replaces it.
    Mock::given(method("GET"))
        .and(path("/api/search"))
        .and(query_param("q", "du"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"results": [result("Stale Du", 1, "none")]}))
                .set_delay(std::time::Duration::from_millis(700)),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/search"))
        .and(query_param("q", "dun"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            result("Dune", 438631, "none"),
            result("Dune: Part Two", 693134, "requested"),
        ]})))
        .mount(&server)
        .await;
    api_json(
        &server,
        "POST",
        "/api/requests",
        201,
        json!({
            "created": true, "remaining_today": 9,
            "request": {"id": 1, "tmdb_id": 438631, "type": "movie", "title": "Dune", "year": 2021,
                        "poster": null, "status": "pending", "created_at": 1700000000}
        }),
    )
    .await;
    // Empty on load, then the new request after submitting.
    let empty = json!({"items": [], "next_cursor": null});
    Mock::given(method("GET"))
        .and(path("/api/requests/mine"))
        .and(query_param("status", "pending"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/requests/mine"))
        .and(query_param("status", "earlier"))
        .respond_with(ResponseTemplate::new(200).set_body_json(empty))
        .mount(&server)
        .await;
    quiet_event_stream(&server, "/api/events").await;
    Mock::given(method("GET"))
        .and(path("/api/requests/mine"))
        .and(query_param("status", "pending"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [
            {"id": 1, "tmdb_id": 438631, "type": "movie", "title": "Dune", "year": 2021,
             "poster": null, "status": "pending", "created_at": 1700000000}
        ], "next_cursor": null})))
        .mount(&server)
        .await;

    let (dom, log) = dump_dom(format!("{}/", server.uri())).await;
    let verdict = dom
        .split(r#"<pre id="e2e-result">"#)
        .nth(1)
        .and_then(|rest| rest.split("</pre>").next())
        .unwrap_or("driver never reported");
    assert_eq!(verdict, "PASS", "\nDOM:\n{dom}\nLOG:\n{log}");
    assert!(
        console_problems(&log).is_empty(),
        "console: {:#?}",
        console_problems(&log)
    );

    // The submit carried the CSRF header and only the expected fields.
    let posts: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.method.as_str() == "POST")
        .collect();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].headers.get("x-requested-with").unwrap(), "mp");
    let body: serde_json::Value = serde_json::from_slice(&posts[0].body).unwrap();
    assert_eq!(body, json!({"tmdb_id": 438631, "media_type": "movie"}));
    // The version placeholder is what the real server substitutes.
    assert_eq!(asset_version().len(), 12);
}

/// Control for the console checks above: an inline script under the CSP
/// must show up as a problem, or the other tests would pass vacuously.
#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn console_check_catches_a_csp_violation() {
    let server = MockServer::start().await;
    let page = "<!doctype html><title>x</title><script>document.title='ran'</script>";
    serve_file(&server, "/", page.as_bytes().to_vec(), "text/html").await;
    let (dom, log) = dump_dom(format!("{}/", server.uri())).await;
    assert!(
        dom.contains("<title>x</title>"),
        "inline script ran despite CSP"
    );
    assert!(
        !console_problems(&log).is_empty(),
        "violation not seen in:\n{log}"
    );
}

#[tokio::test]
#[ignore = "needs a local Chrome"]
async fn admin_queue_archive_and_members_work_end_to_end() {
    let server = MockServer::start().await;
    let page = String::from_utf8(web_file("web/admin.html"))
        .unwrap()
        .replace("__V__", "t")
        .replace(
            "</head>",
            "<script type=\"module\" src=\"/v/t/admin_driver.js\"></script>\n</head>",
        );
    serve_file(
        &server,
        "/admin",
        page.into_bytes(),
        "text/html; charset=utf-8",
    )
    .await;
    for (rel, content_type) in [
        ("js/admin.js", "text/javascript"),
        ("js/api.js", "text/javascript"),
        ("js/live.js", "text/javascript"),
        ("js/ui.js", "text/javascript"),
        ("js/strings.js", "text/javascript"),
        ("js/strings-admin.js", "text/javascript"),
        ("js/theme.js", "text/javascript"),
        ("css/app.css", "text/css"),
        ("css/admin.css", "text/css"),
        ("img/tmdb.svg", "image/svg+xml"),
        ("img/icon.svg", "image/svg+xml"),
    ] {
        serve_file(
            &server,
            &format!("/v/t/{rel}"),
            web_file(&format!("web/{rel}")),
            content_type,
        )
        .await;
    }
    serve_file(
        &server,
        "/v/t/admin_driver.js",
        web_file("tests/js/admin_driver.js"),
        "text/javascript",
    )
    .await;

    let dune = json!({
        "id": 1, "tmdb_id": 438631, "type": "movie", "title": "Dune", "year": 2021,
        "poster": null, "imdb_id": "tt1160419", "dmm_url": "https://dmm.example/movie/tt1160419",
        "note": "the 2021 one", "status": "pending", "requester": "Alice", "times_requested": 2, "infuse_pro": false,
        "created_at": 1700000000, "status_changed_at": 1700000000
    });
    quiet_event_stream(&server, "/api/admin/events").await;
    api_json(
        &server,
        "GET",
        "/api/admin/counts",
        200,
        json!({"pending": 1, "added": 0, "already_had": 0, "refused": 0}),
    )
    .await;
    api_json(&server, "GET", "/api/admin/members", 200, json!({
        "members": [{
            "id": 2, "name": "Bob", "daily_limit": null,
            "created_at": 1700000000, "rotated_at": null, "revoked_at": null,
            "last_seen_at": 1700000100, "request_count": 4,
            "devices": [{"id": 9, "created_at": 1700000000, "last_seen_at": 1700000100, "expires_at": 1800000000}]
        }]
    }))
    .await;
    api_json(
        &server,
        "GET",
        "/api/admin/requests",
        200,
        json!({"items": [dune], "next_cursor": null}),
    )
    .await;
    // The first status change fails, to exercise the rollback.
    Mock::given(method("PATCH"))
        .and(path("/api/admin/requests/1"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"error": "internal_error", "message": "intern feil"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    api_json(
        &server,
        "PATCH",
        "/api/admin/requests/1",
        200,
        json!({"id": 1, "status": "added", "changed": true}),
    )
    .await;
    api_json(
        &server,
        "GET",
        "/api/admin/requests/1/events",
        200,
        json!({"events": [
            {"id": 1, "kind": "submitted", "actor": "Alice", "from_status": null, "to_status": null,
             "note": "the 2021 one", "created_at": 1700000000}
        ]}),
    )
    .await;
    api_json(
        &server,
        "DELETE",
        "/api/admin/requests/1",
        200,
        json!({"ok": true}),
    )
    .await;
    api_json(
        &server,
        "POST",
        "/api/admin/members",
        201,
        json!({"id": 3, "name": "Carol", "code": "ABCD-EFGH"}),
    )
    .await;

    let (dom, log) = dump_dom(format!("{}/admin", server.uri())).await;
    let verdict = dom
        .split(r#"<pre id="e2e-result">"#)
        .nth(1)
        .and_then(|rest| rest.split("</pre>").next())
        .unwrap_or("driver never reported");
    assert_eq!(verdict, "PASS", "\nDOM:\n{dom}\nLOG:\n{log}");
    assert!(
        console_problems(&log).is_empty(),
        "console: {:#?}",
        console_problems(&log)
    );
}
