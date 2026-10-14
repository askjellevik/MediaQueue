//! A stand-in TMDB for load tests (scripts/loadtest.sh), so they never
//! touch the real API. Answers `/search/multi`, `/movie/{id}` and
//! `/tv/{id}` with plausible payloads after a realistic delay.
//!
//! ```text
//! cargo run --example mock_tmdb -- 0.0.0.0:9099
//! ```

use std::{net::TcpListener, time::Duration};

use serde_json::json;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path, path_regex},
};

/// Roughly what a cold TMDB call costs from a home connection.
const UPSTREAM_DELAY: Duration = Duration::from_millis(120);

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9099".into());
    let listener = TcpListener::bind(&addr).expect("bind mock TMDB address");
    let server = MockServer::builder().listener(listener).start().await;

    Mock::given(method("GET"))
        .and(path("/search/multi"))
        .respond_with(|req: &Request| {
            let query = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "query")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let results: Vec<_> = (0..20u32)
                .map(|i| {
                    let id = 1000 + i + 100 * (query.len() as u32);
                    json!({
                        "id": id,
                        "media_type": if i % 3 == 0 { "tv" } else if i % 7 == 0 { "person" } else { "movie" },
                        "title": format!("{query} movie {i}"),
                        "name": format!("{query} series {i}"),
                        "release_date": "2021-09-15",
                        "first_air_date": "2019-01-01",
                        "poster_path": "/poster.jpg",
                        "overview": "An overview long enough to look like a real one. ".repeat(4),
                        "popularity": 10.0,
                    })
                })
                .collect();
            ResponseTemplate::new(200)
                .set_body_json(json!({ "page": 1, "results": results }))
                .set_delay(UPSTREAM_DELAY)
        })
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/(movie|tv)/\d+$"))
        .respond_with(|req: &Request| {
            let id = req.url.path().rsplit('/').next().unwrap_or("0").to_string();
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "id": id.parse::<u32>().unwrap_or(0),
                    "title": format!("Movie {id}"),
                    "name": format!("Series {id}"),
                    "release_date": "2021-09-15",
                    "first_air_date": "2019-01-01",
                    "poster_path": "/poster.jpg",
                    "imdb_id": format!("tt{id:0>7}"),
                    "external_ids": { "imdb_id": format!("tt{id:0>7}") },
                }))
                .set_delay(UPSTREAM_DELAY)
        })
        .mount(&server)
        .await;

    println!("mock TMDB listening on {}", server.uri());
    std::future::pending::<()>().await;
}
