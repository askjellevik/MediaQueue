//! Tests for the admin API, against the real app.

mod common;

use std::collections::HashSet;

use common::{
    HttpResponse, TEST_ADMIN_PASSWORD, TEST_ADMIN_USER, TestApp, cookie_pair, create_member,
    json_post_headers, login_new_member,
};
use serde_json::{Value, json};
use sqlx::SqlitePool;

async fn login_admin(app: &TestApp) -> String {
    let body = format!(r#"{{"username":"{TEST_ADMIN_USER}","password":"{TEST_ADMIN_PASSWORD}"}}"#);
    let response = app
        .request(
            "POST",
            "/api/admin/login",
            &json_post_headers(),
            Some(&body),
        )
        .await;
    assert_eq!(response.status, 200, "{}", response.body);
    cookie_pair(response.header("set-cookie").unwrap()).to_string()
}

/// Sends a request as `cookie` (or anonymously), with the CSRF headers a
/// browser would send for anything state-changing.
async fn call(
    app: &TestApp,
    cookie: Option<&str>,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> HttpResponse {
    let mut headers = json_post_headers();
    if let Some(cookie) = cookie {
        headers.push(("Cookie", cookie));
    }
    app.request(method, path, &headers, body).await
}

fn json_of(response: &HttpResponse) -> Value {
    serde_json::from_str(&response.body).unwrap_or_else(|_| panic!("not json: {}", response.body))
}

async fn member_id(pool: &SqlitePool, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM members WHERE name = ?")
        .bind(name)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Inserts a request with a `submitted` event, like `POST /api/requests`
/// would, and returns its id.
async fn seed_request(
    pool: &SqlitePool,
    requester: (i64, &str),
    tmdb_id: i64,
    title: &str,
    status: &str,
    created_at: i64,
) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO requests (tmdb_id, media_type, imdb_id, title, year, status, requested_by,
                               created_at, status_changed_at)
         VALUES (?, 'movie', ?, ?, 2021, ?, ?, ?, ?) RETURNING id",
    )
    .bind(tmdb_id)
    .bind(format!("tt{tmdb_id:07}"))
    .bind(title)
    .bind(status)
    .bind(requester.0)
    .bind(created_at)
    .bind(created_at)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO request_events (request_id, kind, actor, created_at)
         VALUES (?, 'submitted', ?, ?)",
    )
    .bind(id)
    .bind(requester.1)
    .bind(created_at)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn count(pool: &SqlitePool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// ---------------------------------------------------------------------
// Access control
// ---------------------------------------------------------------------

#[tokio::test]
async fn every_admin_route_is_401_anonymous_and_403_for_members() {
    let app = common::spawn_app().await;
    let user = login_new_member(&app, "Alice").await;
    let routes: [(&str, &str, Option<&str>); 15] = [
        ("POST", "/api/admin/logout", Some("{}")),
        ("GET", "/api/admin/requests", None),
        ("GET", "/api/admin/requests?status=all", None),
        (
            "PATCH",
            "/api/admin/requests/1",
            Some(r#"{"status":"added"}"#),
        ),
        ("DELETE", "/api/admin/requests/1", None),
        ("GET", "/api/admin/requests/1/events", None),
        ("GET", "/api/admin/export.csv", None),
        ("GET", "/api/admin/members", None),
        ("POST", "/api/admin/members", Some(r#"{"name":"Mallory"}"#)),
        (
            "PATCH",
            "/api/admin/members/1",
            Some(r#"{"name":"Mallory"}"#),
        ),
        ("POST", "/api/admin/members/1/rotate", Some("{}")),
        ("POST", "/api/admin/members/1/revoke", Some("{}")),
        ("POST", "/api/admin/members/1/restore", Some("{}")),
        ("DELETE", "/api/admin/members/1", None),
        ("DELETE", "/api/admin/sessions/1", None),
    ];
    for (method, path, body) in routes {
        let anonymous = call(&app, None, method, path, body).await;
        assert_eq!(anonymous.status, 401, "{method} {path}: {}", anonymous.body);
        let member = call(&app, Some(&user), method, path, body).await;
        assert_eq!(member.status, 403, "{method} {path}: {}", member.body);
    }
    // Nothing above changed anything.
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM members").await, 1);
    assert_eq!(
        count(&app.pool, "SELECT COUNT(*) FROM sessions").await,
        1,
        "the member's own session survives"
    );
}

#[tokio::test]
async fn admin_routes_still_need_the_csrf_headers() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let response = app
        .request(
            "POST",
            "/api/admin/members",
            &[("Cookie", &admin), ("Content-Type", "application/json")],
            Some(r#"{"name":"Bob"}"#),
        )
        .await;
    assert_eq!(response.status, 403);
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM members").await, 0);
}

// ---------------------------------------------------------------------
// Status changes and the archive
// ---------------------------------------------------------------------

#[tokio::test]
async fn status_change_writes_exactly_one_event_and_updates_status_changed_at() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (alice, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    let id = seed_request(&app.pool, (alice, "Alice"), 1, "Dune", "pending", 100).await;

    let path = format!("/api/admin/requests/{id}");
    let response = call(
        &app,
        Some(&admin),
        "PATCH",
        &path,
        Some(r#"{"status":"added"}"#),
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        json_of(&response),
        json!({"id": id, "status": "added", "changed": true})
    );

    let (status, changed_at): (String, i64) =
        sqlx::query_as("SELECT status, status_changed_at FROM requests WHERE id = ?")
            .bind(id)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(status, "added");
    assert_eq!(changed_at as u64, app.clock.unix_secs());
    let events: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT kind, actor, from_status, to_status FROM request_events
         WHERE request_id = ? AND kind = 'status'",
    )
    .bind(id)
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        events,
        [(
            "status".into(),
            "admin".into(),
            Some("pending".into()),
            Some("added".into())
        )]
    );

    // Same status again: a no-op, no second event.
    let again = call(
        &app,
        Some(&admin),
        "PATCH",
        &path,
        Some(r#"{"status":"added"}"#),
    )
    .await;
    assert_eq!(json_of(&again)["changed"], false);
    // Any status to any other, including reopening.
    for to in ["refused", "already_had", "pending"] {
        let body = format!(r#"{{"status":"{to}"}}"#);
        assert_eq!(
            call(&app, Some(&admin), "PATCH", &path, Some(&body))
                .await
                .status,
            200
        );
    }
    let status_events = count(
        &app.pool,
        "SELECT COUNT(*) FROM request_events WHERE kind = 'status'",
    )
    .await;
    assert_eq!(status_events, 4);

    // The history endpoint shows it all, oldest first.
    let history = call(&app, Some(&admin), "GET", &format!("{path}/events"), None).await;
    let kinds: Vec<String> = json_of(&history)["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            format!(
                "{}:{}",
                e["kind"].as_str().unwrap(),
                e["to_status"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "submitted:",
            "status:added",
            "status:refused",
            "status:already_had",
            "status:pending"
        ]
    );
}

#[tokio::test]
async fn bad_status_changes_are_rejected() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (alice, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    let id = seed_request(&app.pool, (alice, "Alice"), 1, "Dune", "pending", 100).await;
    let path = format!("/api/admin/requests/{id}");

    for body in [
        r#"{"status":"deleted"}"#,
        r#"{"status":"added","x":1}"#,
        "{}",
    ] {
        assert_eq!(
            call(&app, Some(&admin), "PATCH", &path, Some(body))
                .await
                .status,
            400
        );
    }
    let missing = call(
        &app,
        Some(&admin),
        "PATCH",
        "/api/admin/requests/999",
        Some(r#"{"status":"added"}"#),
    )
    .await;
    assert_eq!(missing.status, 404);
    let not_an_id = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests/abc/events",
        None,
    )
    .await;
    assert_eq!(not_an_id.status, 404);
    assert_eq!(
        not_an_id.body,
        r#"{"error":"not_found","message":"ikke funnet"}"#
    );
    let no_history = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests/999/events",
        None,
    )
    .await;
    assert_eq!(no_history.status, 404);
}

#[tokio::test]
async fn archive_lists_every_request_and_events_cannot_be_deleted_on_their_own() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (alice, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    let mut ids = Vec::new();
    for (i, status) in ["pending", "added", "refused", "already_had", "refused"]
        .iter()
        .enumerate()
    {
        ids.push(
            seed_request(
                &app.pool,
                (alice, "Alice"),
                i as i64 + 1,
                "T",
                status,
                100 + i as i64,
            )
            .await,
        );
    }

    let all = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests?status=all",
        None,
    )
    .await;
    let listed: Vec<i64> = json_of(&all)["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    let mut expected = ids.clone();
    expected.reverse();
    assert_eq!(listed, expected, "newest first, every status");

    let refused = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests?status=refused",
        None,
    )
    .await;
    assert_eq!(json_of(&refused)["items"].as_array().unwrap().len(), 2);
    let pending = call(&app, Some(&admin), "GET", "/api/admin/requests", None).await;
    assert_eq!(
        json_of(&pending)["items"].as_array().unwrap().len(),
        1,
        "pending is the default"
    );

    // A whole request can be deleted (see below), but a single event or
    // the whole list cannot.
    for path in [
        format!("/api/admin/requests/{}/events", ids[0]),
        "/api/admin/requests".to_string(),
    ] {
        let response = call(&app, Some(&admin), "DELETE", &path, None).await;
        assert!(
            response.status >= 400,
            "DELETE {path} gave {}",
            response.status
        );
    }
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM requests").await, 5);
    assert_eq!(
        count(&app.pool, "SELECT COUNT(*) FROM request_events").await,
        5
    );
}

#[tokio::test]
async fn admin_can_delete_a_request_of_any_status_with_its_history() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (alice, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    common::create_member(&app.pool, &app.config, "Bob").await;
    let keep = seed_request(&app.pool, (alice, "Alice"), 1, "Keep", "pending", 100).await;
    let gone = seed_request(&app.pool, (alice, "Alice"), 2, "Gone", "added", 101).await;
    for (kind, actor) in [("duplicate", "Bob"), ("status", "admin")] {
        sqlx::query(
            "INSERT INTO request_events (request_id, kind, actor, created_at) VALUES (?, ?, ?, 102)",
        )
        .bind(gone)
        .bind(kind)
        .bind(actor)
        .execute(&app.pool)
        .await
        .unwrap();
    }

    let response = call(
        &app,
        Some(&admin),
        "DELETE",
        &format!("/api/admin/requests/{gone}"),
        None,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(json_of(&response), json!({ "ok": true }));

    // The request and all three of its events are gone; the other stays.
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM requests").await, 1);
    assert_eq!(
        count(
            &app.pool,
            "SELECT COUNT(*) FROM request_events WHERE request_id = 1"
        )
        .await,
        1
    );
    assert_eq!(
        count(&app.pool, "SELECT COUNT(*) FROM request_events").await,
        1
    );
    let all = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests?status=all",
        None,
    )
    .await;
    let listed: Vec<i64> = json_of(&all)["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(listed, [keep]);

    // Deleting it again, or something that never existed, is a 404.
    for id in [gone, 9999] {
        let again = call(
            &app,
            Some(&admin),
            "DELETE",
            &format!("/api/admin/requests/{id}"),
            None,
        )
        .await;
        assert_eq!(again.status, 404, "{}", again.body);
    }
}

#[tokio::test]
async fn queue_rows_carry_requester_imdb_and_dmm_link_and_filter_by_text() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (alice, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    let (bob, _) = common::create_member(&app.pool, &app.config, "Bob").await;
    seed_request(&app.pool, (alice, "Alice"), 438631, "Dune", "pending", 100).await;
    seed_request(&app.pool, (bob, "Bob"), 2, "100% Wolf", "pending", 101).await;
    sqlx::query("UPDATE requests SET media_type = 'tv' WHERE tmdb_id = 2")
        .execute(&app.pool)
        .await
        .unwrap();

    let page = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests?q=dune",
        None,
    )
    .await;
    let items = json_of(&page)["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "Dune");
    assert_eq!(items[0]["requester"], "Alice");
    assert_eq!(items[0]["imdb_id"], "tt0438631");
    assert_eq!(items[0]["dmm_url"], "https://dmm.example/movie/tt0438631");
    assert_eq!(items[0]["times_requested"], 1);
    assert_eq!(items[0]["infuse_pro"], false);

    // By requester name, by IMDb ID, and with LIKE wildcards taken literally.
    for (q, title) in [
        ("bob", "100% Wolf"),
        ("tt0438631", "Dune"),
        ("100%25", "100% Wolf"),
    ] {
        let page = call(
            &app,
            Some(&admin),
            "GET",
            &format!("/api/admin/requests?q={q}"),
            None,
        )
        .await;
        let items = json_of(&page)["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1, "q={q}");
        assert_eq!(items[0]["title"], title);
    }
    let series = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests?q=wolf",
        None,
    )
    .await;
    assert_eq!(
        json_of(&series)["items"][0]["dmm_url"],
        "https://dmm.example/show/tt0000002"
    );
    let wildcard = call(&app, Some(&admin), "GET", "/api/admin/requests?q=%25", None).await;
    assert_eq!(json_of(&wildcard)["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn pagination_returns_every_row_once_while_new_rows_arrive() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (alice, _) = common::create_member(&app.pool, &app.config, "Alice").await;
    let mut original = HashSet::new();
    // Several rows share a created_at, so the id tie-break matters.
    for (i, created_at) in [100, 100, 100, 101, 102, 102, 103].iter().enumerate() {
        original.insert(
            seed_request(
                &app.pool,
                (alice, "Alice"),
                i as i64 + 1,
                "T",
                "pending",
                *created_at,
            )
            .await,
        );
    }

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    let mut tmdb_id = 100;
    loop {
        let path = match &cursor {
            None => "/api/admin/requests?status=all&limit=3".to_string(),
            Some(c) => format!("/api/admin/requests?status=all&limit=3&cursor={c}"),
        };
        let page = json_of(&call(&app, Some(&admin), "GET", &path, None).await);
        for item in page["items"].as_array().unwrap() {
            seen.push(item["id"].as_i64().unwrap());
        }
        // New requests land between page loads: later than everything, and
        // tied with the newest existing row (a later id breaks the tie).
        for created_at in [200, 103] {
            tmdb_id += 1;
            seed_request(
                &app.pool,
                (alice, "Alice"),
                tmdb_id,
                "New",
                "pending",
                created_at,
            )
            .await;
        }
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }
    let unique: HashSet<i64> = seen.iter().copied().collect();
    assert_eq!(unique.len(), seen.len(), "a row was listed twice: {seen:?}");
    assert_eq!(unique, original);
}

// ---------------------------------------------------------------------
// CSV export
// ---------------------------------------------------------------------

#[tokio::test]
async fn csv_export_streams_the_archive_and_neutralises_formulas() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (evil, _) = common::create_member(&app.pool, &app.config, "@evil").await;
    let titles = [
        "=HYPERLINK(\"http://x\")",
        "+SUM(A1)",
        "-2+3",
        "@cmd",
        "Plain, \"quoted\"",
    ];
    for (i, title) in titles.iter().enumerate() {
        seed_request(
            &app.pool,
            (evil, "@evil"),
            i as i64 + 1,
            title,
            "refused",
            1_700_000_000,
        )
        .await;
    }
    sqlx::query("UPDATE requests SET note = '=1+1' WHERE tmdb_id = 1")
        .execute(&app.pool)
        .await
        .unwrap();

    let response = call(&app, Some(&admin), "GET", "/api/admin/export.csv", None).await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-type"),
        Some("text/csv; charset=utf-8")
    );
    assert!(
        response
            .header("content-disposition")
            .unwrap()
            .starts_with("attachment")
    );
    assert_eq!(
        response.header("transfer-encoding"),
        Some("chunked"),
        "not streamed"
    );
    assert_eq!(response.header("cache-control"), Some("no-store"));

    let body = dechunk(&response.body);
    let mut reader = csv::Reader::from_reader(body.as_bytes());
    let headers: Vec<String> = reader.headers().unwrap().iter().map(String::from).collect();
    assert_eq!(
        headers,
        [
            "id",
            "tmdb_id",
            "type",
            "title",
            "year",
            "imdb_id",
            "status",
            "requested_by",
            "note",
            "times_requested",
            "created_at",
            "status_changed_at",
            "infuse_pro"
        ]
    );
    let rows: Vec<csv::StringRecord> = reader.records().map(Result::unwrap).collect();
    assert_eq!(rows.len(), titles.len());
    for (row, title) in rows.iter().zip(titles) {
        let expected = if title.starts_with(['=', '+', '-', '@']) {
            format!("'{title}")
        } else {
            title.to_string()
        };
        assert_eq!(&row[3], expected);
        assert_eq!(&row[7], "'@evil");
        assert_eq!(&row[10], "2023-11-14T22:13:20Z");
    }
    assert_eq!(&rows[0][8], "'=1+1");
    assert_eq!(&rows[0][6], "refused");
    for row in &rows {
        for cell in row.iter() {
            assert!(
                !cell.starts_with(['=', '+', '-', '@']),
                "unsafe cell {cell:?}"
            );
        }
    }
}

/// Undoes HTTP/1.1 chunked transfer encoding.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, after)) = rest.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        out.push_str(&after[..size]);
        rest = &after[size + 2..];
    }
    out
}

// ---------------------------------------------------------------------
// Members, codes and devices
// ---------------------------------------------------------------------

async fn login_with(app: &TestApp, code: &str) -> HttpResponse {
    let body = format!(r#"{{"code":"{code}"}}"#);
    app.request("POST", "/api/auth/code", &json_post_headers(), Some(&body))
        .await
}

#[tokio::test]
async fn member_lifecycle_create_devices_rotate_revoke() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;

    let created = call(
        &app,
        Some(&admin),
        "POST",
        "/api/admin/members",
        Some(r#"{"name":" Carol "}"#),
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.body);
    let created = json_of(&created);
    assert_eq!(created["name"], "Carol");
    let code = created["code"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 9, "shown as ABCD-EFGH");
    let id = created["id"].as_i64().unwrap();

    let dup = call(
        &app,
        Some(&admin),
        "POST",
        "/api/admin/members",
        Some(r#"{"name":"carol"}"#),
    )
    .await;
    assert_eq!(dup.status, 409);

    // Two devices log in; the admin sees both and kills one.
    let phone =
        cookie_pair(login_with(&app, &code).await.header("set-cookie").unwrap()).to_string();
    let laptop =
        cookie_pair(login_with(&app, &code).await.header("set-cookie").unwrap()).to_string();
    let members = json_of(&call(&app, Some(&admin), "GET", "/api/admin/members", None).await);
    let carol = &members["members"][0];
    assert_eq!(carol["name"], "Carol");
    assert_eq!(carol["daily_limit"], Value::Null);
    let devices = carol["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    let phone_session = devices[0]["id"].as_i64().unwrap();
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "DELETE",
            &format!("/api/admin/sessions/{phone_session}"),
            None
        )
        .await
        .status,
        200
    );
    assert_eq!(
        app.request("GET", "/api/me", &[("Cookie", &phone)], None)
            .await
            .status,
        401
    );
    assert_eq!(
        app.request("GET", "/api/me", &[("Cookie", &laptop)], None)
            .await
            .status,
        200
    );
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "DELETE",
            &format!("/api/admin/sessions/{phone_session}"),
            None
        )
        .await
        .status,
        404
    );

    // Edit: limits, then reset the daily limit to the default with null.
    let path = format!("/api/admin/members/{id}");
    let patched = call(
        &app,
        Some(&admin),
        "PATCH",
        &path,
        Some(r#"{"daily_limit":5,"name":"Caroline"}"#),
    )
    .await;
    assert_eq!(patched.status, 200, "{}", patched.body);
    assert_eq!(member_id(&app.pool, "Caroline").await, id);
    let limit: Option<i64> = sqlx::query_scalar("SELECT daily_limit FROM members WHERE id = ?")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(limit, Some(5));
    call(
        &app,
        Some(&admin),
        "PATCH",
        &path,
        Some(r#"{"daily_limit":null}"#),
    )
    .await;
    let limit: Option<i64> = sqlx::query_scalar("SELECT daily_limit FROM members WHERE id = ?")
        .bind(id)
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(limit, None);
    for bad in [r#"{"daily_limit":-1}"#, r#"{"name":""}"#, r#"{"code":"X"}"#] {
        assert_eq!(
            call(&app, Some(&admin), "PATCH", &path, Some(bad))
                .await
                .status,
            400,
            "{bad}"
        );
    }

    // Rotate: the old code and the remaining device stop working.
    let rotated = call(
        &app,
        Some(&admin),
        "POST",
        &format!("{path}/rotate"),
        Some("{}"),
    )
    .await;
    assert_eq!(rotated.status, 200);
    let new_code = json_of(&rotated)["code"].as_str().unwrap().to_string();
    assert_ne!(new_code, code);
    assert_eq!(
        app.request("GET", "/api/me", &[("Cookie", &laptop)], None)
            .await
            .status,
        401
    );
    assert_eq!(login_with(&app, &code).await.status, 401);
    assert_eq!(login_with(&app, &new_code).await.status, 200);

    // Revoke: the code stops working and every device is logged out.
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "POST",
            &format!("{path}/revoke"),
            Some("{}")
        )
        .await
        .status,
        200
    );
    assert_eq!(login_with(&app, &new_code).await.status, 401);
    assert_eq!(
        count(
            &app.pool,
            "SELECT COUNT(*) FROM sessions WHERE kind = 'user'"
        )
        .await,
        0
    );
    let members = json_of(&call(&app, Some(&admin), "GET", "/api/admin/members", None).await);
    assert!(members["members"][0]["revoked_at"].is_i64());
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "POST",
            &format!("{path}/rotate"),
            Some("{}")
        )
        .await
        .status,
        409,
        "rotating a revoked member"
    );
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "POST",
            "/api/admin/members/999/revoke",
            Some("{}")
        )
        .await
        .status,
        404
    );
}

#[tokio::test]
async fn admin_can_choose_a_members_code_on_create_and_rotate() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let post = |path: String, body: &'static str| {
        let (app, admin) = (&app, &admin);
        async move { call(app, Some(admin), "POST", &path, Some(body)).await }
    };

    // A chosen code logs in however it is typed, and is not echoed back.
    let created = post(
        "/api/admin/members".into(),
        r#"{"name":"Erin","code":"Movie Night"}"#,
    )
    .await;
    assert_eq!(created.status, 201, "{}", created.body);
    assert_eq!(json_of(&created)["code"], Value::Null);
    assert!(!created.body.to_lowercase().contains("movie"));
    assert_eq!(login_with(&app, "movie-night").await.status, 200);
    assert_eq!(login_with(&app, "MOVIENIGHT").await.status, 200);

    // Too short, too long, and already in use; none of them create a member.
    for bad in [r#""ab""#.to_string(), format!("\"{}\"", "x".repeat(65))] {
        let body: &'static str =
            Box::leak(format!(r#"{{"name":"Bad","code":{bad}}}"#).into_boxed_str());
        assert_eq!(post("/api/admin/members".into(), body).await.status, 400);
    }
    let taken = post(
        "/api/admin/members".into(),
        r#"{"name":"Frank","code":"movienight"}"#,
    )
    .await;
    assert_eq!(taken.status, 409);
    assert!(taken.body.contains("koden"));
    assert_eq!(
        count(&app.pool, "SELECT COUNT(*) FROM members").await,
        1,
        "failed creates leave no member behind"
    );

    // Three characters is the shortest allowed, and logs in in lower case.
    let short_ok = post(
        "/api/admin/members".into(),
        r#"{"name":"Hal","code":"Abc"}"#,
    )
    .await;
    assert_eq!(short_ok.status, 201, "{}", short_ok.body);
    assert_eq!(login_with(&app, "abc").await.status, 200);
    assert_eq!(login_with(&app, "ABC").await.status, 200);

    // A blank code still generates one.
    let generated = post(
        "/api/admin/members".into(),
        r#"{"name":"Gina","code":"  "}"#,
    )
    .await;
    assert_eq!(generated.status, 201);
    let generated = json_of(&generated);
    assert_eq!(generated["code"].as_str().unwrap().len(), 9);
    let gina = generated["id"].as_i64().unwrap();

    // Rotate to a chosen code: the old one dies. A duplicate changes nothing.
    let dup = post(
        format!("/api/admin/members/{gina}/rotate"),
        r#"{"code":"movie night"}"#,
    )
    .await;
    assert_eq!(dup.status, 409);
    let short = post(
        format!("/api/admin/members/{gina}/rotate"),
        r#"{"code":"ab"}"#,
    )
    .await;
    assert_eq!(short.status, 400);
    let gina_code = generated["code"].as_str().unwrap();
    assert_eq!(login_with(&app, gina_code).await.status, 200);

    let rotated = post(
        format!("/api/admin/members/{gina}/rotate"),
        r#"{"code":"popcorn time"}"#,
    )
    .await;
    assert_eq!(rotated.status, 200, "{}", rotated.body);
    assert_eq!(json_of(&rotated)["code"], Value::Null);
    assert_eq!(login_with(&app, gina_code).await.status, 401);
    assert_eq!(login_with(&app, "POPCORN-TIME").await.status, 200);

    let tiny = post(
        format!("/api/admin/members/{gina}/rotate"),
        r#"{"code":"xyz"}"#,
    )
    .await;
    assert_eq!(tiny.status, 200, "{}", tiny.body);
    assert_eq!(login_with(&app, "XyZ").await.status, 200);
}

#[tokio::test]
async fn no_endpoint_returns_a_code_except_create_and_rotate() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let created = json_of(
        &call(
            &app,
            Some(&admin),
            "POST",
            "/api/admin/members",
            Some(r#"{"name":"Dave"}"#),
        )
        .await,
    );
    let first = created["code"].as_str().unwrap().to_string();
    let id = created["id"].as_i64().unwrap();
    let rotated = json_of(
        &call(
            &app,
            Some(&admin),
            "POST",
            &format!("/api/admin/members/{id}/rotate"),
            Some("{}"),
        )
        .await,
    );
    let second = rotated["code"].as_str().unwrap().to_string();

    let user = cookie_pair(
        login_with(&app, &second)
            .await
            .header("set-cookie")
            .unwrap(),
    )
    .to_string();
    let (dave, _) = (member_id(&app.pool, "Dave").await, ());
    let request_id = seed_request(&app.pool, (dave, "Dave"), 7, "Dune", "pending", 100).await;

    let reads = [
        (Some(admin.as_str()), "/api/admin/members"),
        (Some(admin.as_str()), "/api/admin/requests?status=all"),
        (Some(admin.as_str()), "/api/admin/export.csv"),
        (
            Some(admin.as_str()),
            &format!("/api/admin/requests/{request_id}/events"),
        ),
        (Some(user.as_str()), "/api/me"),
        (Some(user.as_str()), "/api/requests/mine"),
    ];
    let hash_hex: String = sqlx::query_scalar::<_, Vec<u8>>("SELECT code_hash FROM members")
        .fetch_one(&app.pool)
        .await
        .map(|h| mediaqueue::auth::hex_encode(&h))
        .unwrap();
    for (cookie, path) in reads {
        let response = call(&app, cookie, "GET", path, None).await;
        assert_eq!(response.status, 200, "{path}");
        for code in [&first, &second] {
            let bare = code.replace('-', "");
            assert!(
                !response.body.contains(code.as_str()),
                "{path} leaked a code"
            );
            assert!(!response.body.contains(&bare), "{path} leaked a code");
        }
        assert!(!response.body.contains("code_hash"), "{path}");
        assert!(
            !response.body.contains(&hash_hex),
            "{path} leaked the code hash"
        );
    }
}

#[tokio::test]
async fn revoke_can_be_undone() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (id, code) = create_member(&app.pool, &app.config, "Alice").await;
    let path = format!("/api/admin/members/{id}");

    let revoke = call(
        &app,
        Some(&admin),
        "POST",
        &format!("{path}/revoke"),
        Some("{}"),
    )
    .await;
    assert_eq!(revoke.status, 200);
    assert_eq!(login_with(&app, &code).await.status, 401);

    let restore = call(
        &app,
        Some(&admin),
        "POST",
        &format!("{path}/restore"),
        Some("{}"),
    )
    .await;
    assert_eq!(restore.status, 200, "{}", restore.body);
    assert_eq!(login_with(&app, &code).await.status, 200);
    let members = json_of(&call(&app, Some(&admin), "GET", "/api/admin/members", None).await);
    assert!(members["members"][0]["revoked_at"].is_null());

    let missing = call(
        &app,
        Some(&admin),
        "POST",
        "/api/admin/members/999/restore",
        Some("{}"),
    )
    .await;
    assert_eq!(missing.status, 404);
}

#[tokio::test]
async fn deleting_a_revoked_member_keeps_their_requests_under_their_name() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (alice, alice_code) = create_member(&app.pool, &app.config, "Alice").await;
    let (bob, bob_code) = create_member(&app.pool, &app.config, "Bob").await;
    let delete = |id: i64| format!("/api/admin/members/{id}");

    let dune = seed_request(&app.pool, (alice, "Alice"), 1, "Dune", "pending", 10).await;
    // Alice also asked for something already queued: a duplicate event.
    sqlx::query(
        "INSERT INTO request_events (request_id, kind, actor, created_at)
         VALUES (?, 'duplicate', 'Alice', 11)",
    )
    .bind(dune)
    .execute(&app.pool)
    .await
    .unwrap();
    seed_request(&app.pool, (bob, "Bob"), 2, "Arrival", "pending", 12).await;

    // An active member is never deleted, and revoking comes first.
    let resp = call(&app, Some(&admin), "DELETE", &delete(alice), None).await;
    assert_eq!(resp.status, 409);
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM members").await, 2);

    let path = format!("{}/revoke", delete(alice));
    assert_eq!(
        call(&app, Some(&admin), "POST", &path, Some("{}"))
            .await
            .status,
        200
    );
    let resp = call(&app, Some(&admin), "DELETE", &delete(alice), None).await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    let resp = call(&app, Some(&admin), "DELETE", &delete(alice), None).await;
    assert_eq!(resp.status, 404);

    // Member gone, request kept with the name as text.
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM members").await, 1);
    assert_eq!(count(&app.pool, "SELECT COUNT(*) FROM requests").await, 2);
    let (by, name): (Option<i64>, Option<String>) =
        sqlx::query_as("SELECT requested_by, requested_by_name FROM requests WHERE id = ?")
            .bind(dune)
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!((by, name.as_deref()), (None, Some("Alice")));
    let actors: Vec<String> =
        sqlx::query_scalar("SELECT actor FROM request_events WHERE request_id = ? ORDER BY id")
            .bind(dune)
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(actors, ["Alice (deleted)", "Alice (deleted)"]);

    // Admin views still show who asked.
    let queue = json_of(&call(&app, Some(&admin), "GET", "/api/admin/requests", None).await);
    assert!(
        queue.to_string().contains("\"requester\":\"Alice\""),
        "{queue}"
    );
    let found = call(
        &app,
        Some(&admin),
        "GET",
        "/api/admin/requests?q=alice",
        None,
    )
    .await;
    assert!(found.body.contains("Dune"), "{}", found.body);
    let csv = call(&app, Some(&admin), "GET", "/api/admin/export.csv", None).await;
    assert!(csv.body.contains("Alice"), "{}", csv.body);

    // The old code fails like any wrong code; others are untouched.
    let wrong = login_with(&app, "definitely-wrong").await;
    let gone = login_with(&app, &alice_code).await;
    assert_eq!((gone.status, &gone.body), (wrong.status, &wrong.body));
    assert_eq!(login_with(&app, &bob_code).await.status, 200);

    // A new Alice starts clean: no inherited requests or duplicates.
    let new_alice = call(
        &app,
        Some(&admin),
        "POST",
        "/api/admin/members",
        Some(r#"{"name":"Alice"}"#),
    )
    .await;
    assert_eq!(new_alice.status, 201, "{}", new_alice.body);
    let code = json_of(&new_alice)["code"].as_str().unwrap().to_string();
    let login = login_with(&app, &code).await;
    let cookie = cookie_pair(login.header("set-cookie").unwrap()).to_string();
    let mine = app
        .request("GET", "/api/requests/mine", &[("Cookie", &cookie)], None)
        .await;
    assert_eq!(
        json_of(&mine)["items"].as_array().unwrap().len(),
        0,
        "{}",
        mine.body
    );
}

#[tokio::test]
async fn names_ending_in_deleted_are_reserved() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let create = call(
        &app,
        Some(&admin),
        "POST",
        "/api/admin/members",
        Some(r#"{"name":"Alice (Deleted)"}"#),
    )
    .await;
    assert_eq!(create.status, 409, "{}", create.body);
    let (id, _) = create_member(&app.pool, &app.config, "Bob").await;
    let rename = call(
        &app,
        Some(&admin),
        "PATCH",
        &format!("/api/admin/members/{id}"),
        Some(r#"{"name":"Bob (deleted)"}"#),
    )
    .await;
    assert_eq!(rename.status, 409, "{}", rename.body);
}

#[tokio::test]
async fn renaming_a_member_keeps_their_duplicate_requests() {
    let app = common::spawn_app().await;
    let admin = login_admin(&app).await;
    let (asker, _) = create_member(&app.pool, &app.config, "Alice").await;
    let (bob, _) = create_member(&app.pool, &app.config, "Bob").await;
    let dune = seed_request(&app.pool, (bob, "Bob"), 1, "Dune", "pending", 10).await;
    sqlx::query(
        "INSERT INTO request_events (request_id, kind, actor, created_at)
         VALUES (?, 'duplicate', 'Alice', 11)",
    )
    .bind(dune)
    .execute(&app.pool)
    .await
    .unwrap();
    let rename = call(
        &app,
        Some(&admin),
        "PATCH",
        &format!("/api/admin/members/{asker}"),
        Some(r#"{"name":"Alicia"}"#),
    )
    .await;
    assert_eq!(rename.status, 200, "{}", rename.body);
    let actor: String = sqlx::query_scalar(
        "SELECT actor FROM request_events WHERE request_id = ? AND kind = 'duplicate'",
    )
    .bind(dune)
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(actor, "Alicia");
}
