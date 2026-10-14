//! Serves the embedded `web/` directory (the frontend). The admin HTML
//! carries no data; everything dynamic goes through `/api`.

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;

use crate::auth::hex_encode;

#[derive(Embed)]
#[folder = "web/"]
struct WebAssets;

/// Maps a request path to an embedded asset key. `/` and `/admin` are the
/// only two pages today; everything else is looked up as-is so hashed JS/CSS
/// assets added in later phases need no routing changes here.
fn asset_key(path: &str) -> &str {
    match path.trim_start_matches('/') {
        "" => "index.html",
        "admin" => "admin.html",
        other => other,
    }
}

fn cache_control_for(content_type: &str) -> &'static str {
    if content_type.starts_with("text/html") {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    }
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == etag)
        .unwrap_or(false)
}

pub async fn serve(req: Request) -> Response {
    let key = asset_key(req.uri().path()).to_string();
    let Some(file) = WebAssets::get(&key) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };

    let etag = format!("\"{}\"", hex_encode(&file.metadata.sha256_hash()));
    if if_none_match_matches(req.headers(), &etag) {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let content_type = file.metadata.mimetype();
    let cache_control = cache_control_for(content_type);

    let mut response = file.data.into_owned().into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(content_type).unwrap_or(HeaderValue::from_static("text/plain")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_maps_to_index() {
        assert_eq!(asset_key("/"), "index.html");
    }

    #[test]
    fn admin_maps_to_admin_html() {
        assert_eq!(asset_key("/admin"), "admin.html");
    }

    #[test]
    fn other_paths_pass_through() {
        assert_eq!(asset_key("/js/search.js"), "js/search.js");
    }

    #[test]
    fn cache_control_differs_by_type() {
        assert_eq!(cache_control_for("text/html; charset=utf-8"), "no-cache");
        assert_eq!(
            cache_control_for("application/javascript"),
            "public, max-age=31536000, immutable"
        );
    }
}
