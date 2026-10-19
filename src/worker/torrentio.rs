//! The Torrentio candidate source: one plain GET per job, no Real-Debrid
//! key and no configuration in the URL, so what comes back is the public,
//! cacheable answer.
//!
//! Everything in the response is untrusted. Each item is checked strictly
//! and an item that fails any check is dropped, never repaired. If a
//! response has items and every one of them fails, the format has most
//! likely changed, and that is an error rather than "no candidates".

use std::time::{Duration, Instant};

use reqwest::{StatusCode, redirect::Policy};
use serde_json::Value;

use super::{
    candidate::RawCandidate,
    source::{CandidateSource, MediaKind, SourceError},
    torrentio_text::parse_stream_text,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_STREAMS: usize = 100;
const MAX_TITLE_CHARS: usize = 1_000;
const MAX_NAME_CHARS: usize = 100;
const MAX_FILENAME_CHARS: usize = 500;

#[derive(Debug, Clone)]
pub struct TorrentioSource {
    client: reqwest::Client,
    base_url: String,
}

impl TorrentioSource {
    pub fn new(base_url: &str) -> Result<Self, reqwest::Error> {
        Self::with_timeouts(base_url, CONNECT_TIMEOUT, REQUEST_TIMEOUT)
    }

    /// Separate from `new` so a test can wait for a short timeout.
    pub fn with_timeouts(
        base_url: &str,
        connect: Duration,
        request: Duration,
    ) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("mediaqueue-worker/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(connect)
            .timeout(request)
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }
}

impl CandidateSource for TorrentioSource {
    async fn candidates(
        &self,
        imdb_id: &str,
        kind: MediaKind,
    ) -> Result<Vec<RawCandidate>, SourceError> {
        let MediaKind::Movie = kind;
        // Nothing reaches the URL path unchecked.
        if !is_imdb_id(imdb_id) {
            return Err(SourceError::BadResponse("not an imdb id".into()));
        }
        let started = Instant::now();
        let url = format!("{}/stream/movie/{imdb_id}.json", self.base_url);
        let body = self.fetch(&url).await?;
        let parsed = parse_response(&body)?;
        tracing::info!(
            imdb_id,
            received = parsed.received,
            cut = parsed.cut,
            dropped = parsed.dropped,
            kept = parsed.kept.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "torrentio fetched"
        );
        if parsed.dropped > 0 && parsed.kept.is_empty() {
            return Err(SourceError::BadResponse(
                "no valid items; response format changed?".into(),
            ));
        }
        Ok(parsed.kept)
    }
}

impl TorrentioSource {
    /// The body of a 200 answer, at most `MAX_BODY_BYTES` long.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, SourceError> {
        let mut response = self.client.get(url).send().await.map_err(unreachable)?;

        let status = response.status();
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(SourceError::Unreachable(format!(
                "status {}",
                status.as_u16()
            )));
        }
        if status != StatusCode::OK {
            return Err(SourceError::BadResponse(format!(
                "status {}",
                status.as_u16()
            )));
        }
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BODY_BYTES as u64)
        {
            return Err(SourceError::BadResponse("too large".into()));
        }

        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(unreachable)? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(SourceError::BadResponse("too large".into()));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// A short reason with no URL and no body in it.
fn unreachable(e: reqwest::Error) -> SourceError {
    let why = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect failed"
    } else {
        "request failed"
    };
    SourceError::Unreachable(why.into())
}

/// `tt` and then 5 to 10 digits.
fn is_imdb_id(id: &str) -> bool {
    id.strip_prefix("tt").is_some_and(|digits| {
        (5..=10).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

struct Parsed {
    /// Items in the response.
    received: usize,
    /// Items beyond the first `MAX_STREAMS`, never looked at.
    cut: usize,
    /// Items among the first `MAX_STREAMS` that failed a check.
    dropped: usize,
    kept: Vec<RawCandidate>,
}

fn parse_response(body: &[u8]) -> Result<Parsed, SourceError> {
    let bad = |why: &str| SourceError::BadResponse(why.into());
    let value: Value = serde_json::from_slice(body).map_err(|_| bad("not json"))?;
    let streams = value
        .get("streams")
        .ok_or_else(|| bad("no streams"))?
        .as_array()
        .ok_or_else(|| bad("streams is not a list"))?;

    let received = streams.len();
    let mut dropped = 0;
    let mut kept = Vec::new();
    for stream in streams.iter().take(MAX_STREAMS) {
        match candidate_from(stream) {
            Some(candidate) => kept.push(candidate),
            None => dropped += 1,
        }
    }
    Ok(Parsed {
        received,
        cut: received.saturating_sub(MAX_STREAMS),
        dropped,
        kept,
    })
}

/// `None` when the item fails any check. Only `infoHash`, `name`, `title`
/// and `behaviorHints.filename` are read; `fileIdx`, `sources` and
/// `bingeGroup` are ignored and never stored.
fn candidate_from(stream: &Value) -> Option<RawCandidate> {
    let infohash = stream.get("infoHash")?.as_str()?;
    if infohash.len() != 40 || !infohash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let title = bounded(stream.get("title")?, MAX_TITLE_CHARS)?;
    let name = bounded(stream.get("name")?, MAX_NAME_CHARS)?;
    let filename = match stream.get("behaviorHints").and_then(|h| h.get("filename")) {
        None | Some(Value::Null) => None,
        Some(value) => Some(bounded(value, MAX_FILENAME_CHARS)?),
    };

    // The line breaks matter and are split on inside the text parser.
    parse_stream_text(name, title, filename)
        .map(|parts| parts.into_candidate(infohash.to_ascii_lowercase()))
}

fn bounded(value: &Value, max_chars: usize) -> Option<&str> {
    value.as_str().filter(|s| s.chars().count() <= max_chars)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    use super::*;

    const IMDB: &str = "tt1234567";
    const FIXTURE: &str = include_str!("../../tests/fixtures/torrentio_example.json");

    fn hash(n: u32) -> String {
        format!("{n:040x}")
    }

    fn stream(n: u32) -> Value {
        json!({
            "name": "Torrentio\n1080p",
            "title": format!("Example.Movie.1994.1080p.BluRay.x264-GRP{n}\n👤 10 💾 4.2 GB ⚙️ ExampleSite"),
            "infoHash": hash(n),
            "fileIdx": 0,
        })
    }

    fn response(streams: Vec<Value>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "streams": streams, "cacheMaxAge": 1 }))
    }

    async fn serve(template: ResponseTemplate) -> (MockServer, TorrentioSource) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/stream/movie/{IMDB}.json")))
            .respond_with(template)
            .mount(&server)
            .await;
        let source = TorrentioSource::new(&server.uri()).unwrap();
        (server, source)
    }

    async fn ask(source: &TorrentioSource) -> Result<Vec<RawCandidate>, SourceError> {
        source.candidates(IMDB, MediaKind::Movie).await
    }

    async fn status_gives(code: u16) -> Result<Vec<RawCandidate>, SourceError> {
        let (_server, source) = serve(ResponseTemplate::new(code)).await;
        ask(&source).await
    }

    #[tokio::test]
    async fn the_fixture_reads_into_its_fourteen_candidates() {
        let (_server, source) =
            serve(ResponseTemplate::new(200).set_body_raw(FIXTURE.as_bytes(), "application/json"))
                .await;
        let candidates = ask(&source).await.unwrap();
        assert_eq!(candidates.len(), 14);
        assert_eq!(candidates[0].infohash, hash(1));
        assert_eq!(candidates[0].seeders, Some(120));
    }

    #[tokio::test]
    async fn the_request_is_plain_and_names_the_worker() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/stream/movie/{IMDB}.json")))
            .and(header(
                "user-agent",
                concat!("mediaqueue-worker/", env!("CARGO_PKG_VERSION")),
            ))
            .respond_with(response(vec![stream(1)]))
            .expect(1)
            .mount(&server)
            .await;
        // A trailing slash on the base does not double up.
        let source = TorrentioSource::new(&format!("{}/", server.uri())).unwrap();
        assert_eq!(ask(&source).await.unwrap().len(), 1);
        let requests = server.received_requests().await.unwrap();
        assert!(requests[0].headers.get("authorization").is_none());
        assert!(requests[0].headers.get("cookie").is_none());
    }

    #[tokio::test]
    async fn server_errors_and_rate_limits_are_unreachable() {
        for code in [500, 503, 429] {
            assert!(
                matches!(status_gives(code).await, Err(SourceError::Unreachable(_))),
                "{code}"
            );
        }
    }

    #[tokio::test]
    async fn other_statuses_and_redirects_are_a_bad_response() {
        for code in [404, 403, 301, 302] {
            assert!(
                matches!(status_gives(code).await, Err(SourceError::BadResponse(_))),
                "{code}"
            );
        }
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let server = MockServer::start().await;
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(301)
                    .insert_header("location", format!("{}/elsewhere", target.uri()).as_str()),
            )
            .mount(&server)
            .await;
        let source = TorrentioSource::new(&server.uri()).unwrap();
        assert!(matches!(
            ask(&source).await,
            Err(SourceError::BadResponse(_))
        ));
        assert!(target.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_slow_answer_is_unreachable() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(response(vec![stream(1)]).set_delay(Duration::from_millis(600)))
            .mount(&server)
            .await;
        let source = TorrentioSource::with_timeouts(
            &server.uri(),
            Duration::from_secs(1),
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(matches!(
            ask(&source).await,
            Err(SourceError::Unreachable(_))
        ));
    }

    #[tokio::test]
    async fn a_refused_connection_is_unreachable() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let source = TorrentioSource::new(&format!("http://127.0.0.1:{port}")).unwrap();
        let error = ask(&source).await.unwrap_err();
        assert!(matches!(error, SourceError::Unreachable(_)));
        assert!(
            !error.to_string().contains("127.0.0.1"),
            "no URL in the error"
        );
    }

    #[tokio::test]
    async fn a_response_of_the_wrong_shape_is_a_bad_response() {
        let bodies: [(&str, ResponseTemplate); 5] = [
            (
                "not json",
                ResponseTemplate::new(200).set_body_string("<html>"),
            ),
            (
                "no streams",
                ResponseTemplate::new(200).set_body_json(json!({"a": 1})),
            ),
            (
                "streams not a list",
                ResponseTemplate::new(200).set_body_json(json!({"streams": {}})),
            ),
            (
                "top level list",
                ResponseTemplate::new(200).set_body_json(json!([])),
            ),
            ("empty body", ResponseTemplate::new(200)),
        ];
        for (what, template) in bodies {
            let (_server, source) = serve(template).await;
            assert!(
                matches!(ask(&source).await, Err(SourceError::BadResponse(_))),
                "{what}"
            );
        }
    }

    #[tokio::test]
    async fn a_body_over_one_mebibyte_is_refused() {
        let big = vec![b' '; 1_200_000];
        let (_server, source) = serve(ResponseTemplate::new(200).set_body_bytes(big)).await;
        assert_eq!(
            ask(&source).await,
            Err(SourceError::BadResponse("too large".into()))
        );
    }

    #[tokio::test]
    async fn a_body_just_under_the_cap_is_read() {
        let mut body = serde_json::to_vec(&json!({ "streams": [stream(1)] })).unwrap();
        body.resize(MAX_BODY_BYTES, b' ');
        let (_server, source) = serve(ResponseTemplate::new(200).set_body_bytes(body)).await;
        assert_eq!(ask(&source).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn at_most_a_hundred_streams_are_taken() {
        let streams = (1..=101).map(stream).collect();
        let (_server, source) = serve(response(streams)).await;
        let candidates = ask(&source).await.unwrap();
        assert_eq!(candidates.len(), 100);
        assert_eq!(candidates[99].infohash, hash(100));
    }

    #[tokio::test]
    async fn good_items_survive_bad_neighbours() {
        let mut short_hash = stream(2);
        short_hash["infoHash"] = json!("abcdef");
        let mut not_hex = stream(3);
        not_hex["infoHash"] = json!("z".repeat(40));
        let mut no_stats = stream(4);
        no_stats["title"] = json!("Example.Movie.1994.1080p\nno statistics here");
        let (_server, source) = serve(response(vec![
            stream(1),
            short_hash,
            not_hex,
            no_stats,
            json!("not an object"),
            stream(5),
        ]))
        .await;
        let kept: Vec<String> = ask(&source)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.infohash)
            .collect();
        assert_eq!(kept, [hash(1), hash(5)]);
    }

    #[tokio::test]
    async fn an_item_with_an_oversized_field_is_dropped() {
        let mut long_title = stream(2);
        long_title["title"] = json!(format!("{}\n👤 1 💾 1 GB ⚙️ X", "a".repeat(1_000)));
        let mut long_name = stream(3);
        long_name["name"] = json!("n".repeat(101));
        let mut long_file = stream(4);
        long_file["behaviorHints"] = json!({ "filename": "f".repeat(501) });
        let mut odd_file = stream(5);
        odd_file["behaviorHints"] = json!({ "filename": 7 });
        let (_server, source) = serve(response(vec![
            stream(1),
            long_title,
            long_name,
            long_file,
            odd_file,
        ]))
        .await;
        let kept = ask(&source).await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].infohash, hash(1));
    }

    #[tokio::test]
    async fn fields_at_their_limits_are_kept() {
        let mut edge = stream(1);
        edge["name"] = json!("n".repeat(100));
        edge["behaviorHints"] = json!({ "filename": "f".repeat(500) });
        let stats = "\n👤 1 💾 1 GB ⚙️ X";
        edge["title"] = json!(format!(
            "{}{stats}",
            "a".repeat(1_000 - stats.chars().count())
        ));
        let (_server, source) = serve(response(vec![edge])).await;
        assert_eq!(ask(&source).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_infohash_is_lowercased() {
        let mut upper = stream(0xabc);
        upper["infoHash"] = json!(hash(0xabc).to_uppercase());
        let (_server, source) = serve(response(vec![upper])).await;
        assert_eq!(ask(&source).await.unwrap()[0].infohash, hash(0xabc));
    }

    #[tokio::test]
    async fn control_characters_go_but_the_lines_stay_apart() {
        let mut item = stream(1);
        item["title"] = json!(
            "Example.Mo\u{7}vie.1994.1080p.BluRay.x264-GRP\r\n👤 10\u{0} 💾 4.2 GB ⚙️ Example\u{1b}Site"
        );
        let (_server, source) = serve(response(vec![item])).await;
        let c = ask(&source).await.unwrap().remove(0);
        assert_eq!(c.name, "Example.Movie.1994.1080p.BluRay.x264-GRP");
        assert_eq!(c.seeders, Some(10));
        assert_eq!(c.provider.as_deref(), Some("ExampleSite"));
        assert!(!c.multi_file);
    }

    #[tokio::test]
    async fn every_item_bad_is_a_changed_format_not_an_empty_list() {
        let mut broken = stream(1);
        broken["infoHash"] = json!(null);
        let (_server, source) = serve(response(vec![broken, json!({}), json!(3)])).await;
        assert_eq!(
            ask(&source).await,
            Err(SourceError::BadResponse(
                "no valid items; response format changed?".into()
            ))
        );
    }

    #[tokio::test]
    async fn an_empty_streams_list_is_a_valid_nothing() {
        let (_server, source) = serve(response(vec![])).await;
        assert_eq!(ask(&source).await, Ok(vec![]));
    }

    #[tokio::test]
    async fn a_bad_imdb_id_never_makes_a_request() {
        let (server, source) = serve(response(vec![stream(1)])).await;
        for bad in [
            "",
            "tt",
            "tt1234",
            "tt12345678901",
            "1234567",
            "tt12a4567",
            "TT1234567",
            "tt1234567/../x",
            "tt1234567?x=1",
            " tt1234567",
        ] {
            assert!(
                matches!(
                    source.candidates(bad, MediaKind::Movie).await,
                    Err(SourceError::BadResponse(_))
                ),
                "{bad:?}"
            );
        }
        assert!(server.received_requests().await.unwrap().is_empty());
        // The bounds themselves are fine.
        assert!(is_imdb_id("tt12345") && is_imdb_id("tt1234567890"));
    }
}
