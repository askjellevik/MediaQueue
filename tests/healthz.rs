mod common;

#[tokio::test]
async fn healthz_returns_ok_true() {
    let app = common::spawn_app().await;
    let response = common::http_get(app.addr, "/healthz").await;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected status line in: {response}"
    );
    assert!(
        response.contains(r#"{"ok":true}"#),
        "unexpected body in: {response}"
    );
}

#[tokio::test]
async fn unknown_path_returns_404() {
    let app = common::spawn_app().await;
    let response = common::http_get(app.addr, "/does-not-exist").await;

    assert!(
        response.starts_with("HTTP/1.1 404"),
        "unexpected status line in: {response}"
    );
}

#[tokio::test]
async fn root_serves_embedded_index_html_with_no_cache() {
    let app = common::spawn_app().await;
    let response = common::http_get(app.addr, "/").await;

    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(
        response
            .to_ascii_lowercase()
            .contains("content-type: text/html")
    );
    assert!(
        response
            .to_ascii_lowercase()
            .contains("cache-control: no-cache")
    );
    assert!(response.contains("Media requests portal"));
}
