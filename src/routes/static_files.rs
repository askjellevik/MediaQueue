//! Serves the embedded `web/` directory (the frontend). The admin HTML
//! carries no data; everything dynamic goes through `/api`.
//!
//! Cache busting without a build step: HTML pages reference assets as
//! `/v/__V__/js/app.js`, and the server replaces `__V__` with a hash over
//! every embedded file. Only a URL carrying the current hash is served as
//! `immutable`; anything else is `no-cache` and revalidated by ETag, so a
//! deploy can never leave a browser on stale JS. Relative module imports
//! (`./api.js`) inherit the versioned prefix automatically.

use std::{borrow::Cow, sync::OnceLock};

use axum::{
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;
use sha2::{Digest, Sha256};

use crate::auth::hex_encode;

#[derive(Embed)]
#[folder = "web/"]
struct WebAssets;

const VERSION_PLACEHOLDER: &str = "__V__";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// A short hash over every embedded file, computed once. Changes whenever
/// any asset does.
pub fn asset_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let mut names: Vec<Cow<'static, str>> = WebAssets::iter().collect();
        names.sort();
        let mut hasher = Sha256::new();
        for name in names {
            if let Some(file) = WebAssets::get(&name) {
                hasher.update(name.as_bytes());
                hasher.update(file.metadata.sha256_hash());
            }
        }
        hex_encode(&hasher.finalize()[..6])
    })
}

/// Where a request path points: the embedded file, and whether the URL
/// carried the current asset version.
struct Target<'a> {
    key: &'a str,
    versioned: bool,
}

fn resolve(path: &str) -> Target<'_> {
    let path = path.trim_start_matches('/');
    if let Some(rest) = path.strip_prefix("v/") {
        if let Some((version, key)) = rest.split_once('/') {
            return Target {
                key,
                versioned: version == asset_version(),
            };
        }
    }
    let key = match path {
        "" => "index.html",
        "admin" => "admin.html",
        other => other,
    };
    Target {
        key,
        versioned: false,
    }
}

fn is_html(content_type: &str) -> bool {
    content_type.starts_with("text/html")
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == etag)
        .unwrap_or(false)
}

pub async fn serve(req: Request) -> Response {
    let target = resolve(req.uri().path());
    let Some(file) = WebAssets::get(target.key) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let content_type = file.metadata.mimetype();
    let html = is_html(content_type);

    // HTML depends on the asset version too, so its ETag covers both.
    let mut tag_input = file.metadata.sha256_hash().to_vec();
    if html {
        tag_input.extend_from_slice(asset_version().as_bytes());
    }
    let etag = format!("\"{}\"", hex_encode(&Sha256::digest(&tag_input)[..16]));
    if if_none_match_matches(req.headers(), &etag) {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    let body = if html {
        String::from_utf8_lossy(&file.data)
            .replace(VERSION_PLACEHOLDER, asset_version())
            .into_bytes()
    } else {
        file.data.into_owned()
    };
    let cache_control = if target.versioned && !html {
        IMMUTABLE
    } else {
        "no-cache"
    };

    let mut response = body.into_response();
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
    fn root_and_admin_map_to_pages() {
        assert_eq!(resolve("/").key, "index.html");
        assert_eq!(resolve("/admin").key, "admin.html");
        assert!(!resolve("/").versioned);
    }

    #[test]
    fn other_paths_pass_through_unversioned() {
        let t = resolve("/js/search.js");
        assert_eq!(t.key, "js/search.js");
        assert!(!t.versioned);
    }

    #[test]
    fn versioned_paths_strip_the_prefix() {
        let current = format!("/v/{}/js/search.js", asset_version());
        let t = resolve(&current);
        assert_eq!(t.key, "js/search.js");
        assert!(t.versioned);

        let old = resolve("/v/0000/js/search.js");
        assert_eq!(old.key, "js/search.js");
        assert!(!old.versioned);
    }

    #[test]
    fn version_is_short_hex() {
        let v = asset_version();
        assert_eq!(v.len(), 12);
        assert!(v.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
