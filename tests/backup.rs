//! Phase 7: a backup restores into a working app, and only the newest 14
//! are kept.

mod common;

use common::{cookie_pair, json_post_headers, login_new_member};
use mediaqueue::tasks;
use serde_json::Value;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn backup_restores_into_a_working_app() {
    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/438631"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "title": "Dune", "release_date": "2021-09-15", "imdb_id": "tt1160419"
        })))
        .mount(&tmdb)
        .await;
    let uri = tmdb.uri();
    let app = common::spawn_app_with(move |cfg| cfg.tmdb_base_url = uri).await;

    // Real data through the API: a member, their code and a request.
    let (_, code) = common::create_member(&app.pool, &app.config, "Alice").await;
    let login = app
        .request(
            "POST",
            "/api/auth/code",
            &json_post_headers(),
            Some(&format!(r#"{{"code":"{code}"}}"#)),
        )
        .await;
    let cookie = cookie_pair(login.header("set-cookie").unwrap()).to_string();
    let mut headers = json_post_headers();
    headers.push(("Cookie", &cookie));
    let created = app
        .request(
            "POST",
            "/api/requests",
            &headers,
            Some(r#"{"tmdb_id":438631,"media_type":"movie","note":"keep me"}"#),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.body);

    // A directory of its own per test run (the real one is `backup_dir`).
    let dir = app.config.database_path.with_extension("backups");
    let backup = tasks::backup_now(&app.pool, &dir, 1_700_000_000)
        .await
        .unwrap();
    assert!(
        backup.ends_with("db-20231114T221320Z.sqlite3"),
        "{backup:?}"
    );

    // Restore: copy the backup into place as a fresh database file and
    // start a second app on it, the same pepper and secrets as before.
    let restored_path = app.config.database_path.with_extension("restored.sqlite3");
    std::fs::copy(&backup, &restored_path).unwrap();
    let restored = common::spawn_app_with(|cfg| {
        cfg.database_path = restored_path.clone();
    })
    .await;

    assert_eq!(
        restored.request("GET", "/healthz", &[], None).await.status,
        200
    );
    // The member's code still works, and their request is there.
    let login = restored
        .request(
            "POST",
            "/api/auth/code",
            &json_post_headers(),
            Some(&format!(r#"{{"code":"{code}"}}"#)),
        )
        .await;
    assert_eq!(login.status, 200, "{}", login.body);
    let cookie = cookie_pair(login.header("set-cookie").unwrap()).to_string();
    let mine = restored
        .request("GET", "/api/requests/mine", &[("Cookie", &cookie)], None)
        .await;
    let mine: Value = serde_json::from_str(&mine.body).unwrap();
    assert_eq!(mine["items"][0]["title"], "Dune");
    let note: String = sqlx::query_scalar("SELECT note FROM requests")
        .fetch_one(&restored.pool)
        .await
        .unwrap();
    assert_eq!(note, "keep me");
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_events")
        .fetch_one(&restored.pool)
        .await
        .unwrap();
    assert_eq!(events, 1);
    // A restored database accepts new writes too.
    let _ = login_new_member(&restored, "Bob").await;
}

#[tokio::test]
async fn only_the_newest_fourteen_backups_are_kept() {
    let app = common::spawn_app().await;
    let dir = app.config.database_path.with_extension("backups");
    let day = 86_400;
    let mut written = Vec::new();
    for n in 0..16 {
        written.push(
            tasks::backup_now(&app.pool, &dir, 1_700_000_000 + n * day)
                .await
                .unwrap(),
        );
    }
    // An unrelated file in the directory is left alone.
    std::fs::write(dir.join("notes.txt"), "mine").unwrap();
    tasks::backup_now(&app.pool, &dir, 1_700_000_000 + 16 * day)
        .await
        .unwrap();

    let mut left: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left.len(), tasks::BACKUPS_KEPT + 1);
    assert!(left.contains(&"notes.txt".to_string()));
    assert!(!written[0].exists() && !written[1].exists() && !written[2].exists());
    assert!(written[3].exists());
    assert_eq!(left[0], "db-20231117T221320Z.sqlite3");
}
