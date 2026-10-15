//! Phase 5 checks on the embedded frontend that need no browser: what the
//! page contains before login, CSP-friendliness, caching of versioned
//! assets and the transfer budget. Browser-only checks (keyboard flow,
//! console under CSP, Lighthouse) are in docs/frontend-checks.md.

mod common;

use common::TestApp;
use mediaqueue::routes::static_files::asset_version;

const PAGE_ASSETS: [&str; 7] = [
    "css/app.css",
    "js/search.js",
    "js/api.js",
    "js/live.js",
    "js/ui.js",
    "img/tmdb.svg",
    "img/icon.svg",
];

async fn get(app: &TestApp, path: &str) -> common::HttpResponse {
    app.request("GET", path, &[], None).await
}

fn versioned(asset: &str) -> String {
    format!("/v/{}/{asset}", asset_version())
}

/// True if the markup has an `on<event>=` attribute such as `onclick=`.
fn has_event_attribute(html: &str) -> bool {
    html.match_indices(" on").any(|(i, _)| {
        let rest = &html[i + 3..];
        let name_len = rest.bytes().take_while(u8::is_ascii_lowercase).count();
        name_len > 0 && rest[name_len..].trim_start().starts_with('=')
    })
}

#[tokio::test]
async fn admin_page_carries_no_data_and_no_inline_code() {
    let app = common::spawn_app().await;
    let page = get(&app, "/admin").await;
    assert_eq!(page.status, 200);
    assert_eq!(page.header("cache-control"), Some("no-cache"));
    let html = page.body.to_ascii_lowercase();
    assert!(html.contains(&versioned("js/admin.js")));
    for marker in ["<table", "<input", "<form", "tt0", "<style", " style="] {
        assert!(!html.contains(marker), "admin.html contains {marker:?}");
    }
    for tag in html.split("<script").skip(1) {
        assert!(tag.split('>').next().unwrap_or("").contains("src="));
    }
    assert!(!has_event_attribute(&html));
}

#[tokio::test]
async fn index_has_no_search_ui_before_login() {
    let app = common::spawn_app().await;
    let page = get(&app, "/").await;
    assert_eq!(page.status, 200);
    let html = page.body.to_ascii_lowercase();
    for marker in [
        "<input",
        "<form",
        "combobox",
        "listbox",
        "search movies",
        "<dialog",
    ] {
        assert!(!html.contains(marker), "index.html contains {marker:?}");
    }
}

#[tokio::test]
async fn index_references_assets_by_current_version_and_has_no_inline_code() {
    let app = common::spawn_app().await;
    let html = get(&app, "/").await.body;
    assert!(!html.contains("__V__"), "placeholder left in page");
    assert!(html.contains(&versioned("js/search.js")));
    assert!(html.contains(&versioned("css/app.css")));

    // CSP allows no inline script or style: every script has a src, and
    // there are no style blocks or attributes.
    let lower = html.to_ascii_lowercase();
    for tag in lower.split("<script").skip(1) {
        let open = tag.split('>').next().unwrap_or("");
        assert!(open.contains("src="), "inline <script> found");
    }
    assert!(!lower.contains("<style"));
    assert!(!lower.contains(" style="));
    assert!(
        !has_event_attribute(&lower),
        "inline event handler attribute"
    );
}

#[tokio::test]
async fn scripts_never_write_html() {
    let app = common::spawn_app().await;
    for asset in [
        "js/search.js",
        "js/api.js",
        "js/live.js",
        "js/ui.js",
        "js/admin.js",
    ] {
        let js = get(&app, &versioned(asset)).await.body;
        assert!(!js.is_empty(), "{asset} missing");
        for banned in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "eval(",
            "new Function",
            ".style.",
        ] {
            assert!(!js.contains(banned), "{asset} uses {banned}");
        }
    }
}

#[tokio::test]
async fn module_imports_resolve() {
    let app = common::spawn_app().await;
    for asset in [
        "js/search.js",
        "js/api.js",
        "js/live.js",
        "js/ui.js",
        "js/admin.js",
    ] {
        let js = get(&app, &versioned(asset)).await.body;
        for line in js.lines().filter(|l| l.contains(" from './")) {
            let target = line
                .split("from './")
                .nth(1)
                .and_then(|rest| rest.split('\'').next())
                .expect("import path");
            let response = get(&app, &versioned(&format!("js/{target}"))).await;
            assert_eq!(response.status, 200, "{asset} imports missing {target}");
        }
    }
}

#[tokio::test]
async fn versioned_assets_are_immutable_and_others_revalidate() {
    let app = common::spawn_app().await;
    for (asset, content_type) in [
        ("js/search.js", "javascript"),
        ("css/app.css", "text/css"),
        ("img/tmdb.svg", "image/svg+xml"),
    ] {
        let fresh = get(&app, &versioned(asset)).await;
        assert_eq!(fresh.status, 200, "{asset}");
        assert!(
            fresh
                .header("content-type")
                .unwrap_or("")
                .contains(content_type),
            "{asset}: {:?}",
            fresh.header("content-type")
        );
        assert_eq!(
            fresh.header("cache-control"),
            Some("public, max-age=31536000, immutable")
        );
        assert!(fresh.header("content-security-policy").is_some());

        let etag = fresh.header("etag").expect("etag").to_string();
        let revalidated = app
            .request("GET", &versioned(asset), &[("If-None-Match", &etag)], None)
            .await;
        assert_eq!(revalidated.status, 304);

        let plain = get(&app, &format!("/{asset}")).await;
        assert_eq!(plain.header("cache-control"), Some("no-cache"));
        let stale_version = get(&app, &format!("/v/000000000000/{asset}")).await;
        assert_eq!(stale_version.header("cache-control"), Some("no-cache"));
    }
    assert_eq!(
        get(&app, "/").await.header("cache-control"),
        Some("no-cache")
    );
    assert_eq!(get(&app, "/v/x/nope.js").await.status, 404);
}

#[tokio::test]
async fn main_page_transfer_is_under_50_kb() {
    let app = common::spawn_app().await;
    let mut total = get(&app, "/").await.body.len();
    for asset in PAGE_ASSETS {
        let response = get(&app, &versioned(asset)).await;
        assert_eq!(response.status, 200, "{asset}");
        total += response.body.len();
    }
    assert!(total < 50 * 1024, "main page assets total {total} bytes");
}
