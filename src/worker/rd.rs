//! The Real-Debrid client: a thin, typed layer over the handful of REST
//! calls the worker needs. It decides nothing about which torrent to add.
//!
//! What it does enforce: every call first asks the circuit breaker and then
//! takes a token from a request budget; a response that trips the breaker is
//! recorded in the state file before the call returns; failures are sorted by
//! the numeric `error_code` in the response body first and by the HTTP status
//! only when the body has no code this module knows; the token lives in a
//! [`Secret`] that prints as nothing and is sent only as a sensitive header.
//! There are no retries here.

use std::{fmt, sync::Arc, time::Duration};

use reqwest::{
    Method, RequestBuilder, StatusCode, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Deserializer};
use tokio::{sync::Mutex, time::Instant};

use super::{
    config::RdConfig,
    state::{Limits, StateStore, SystemClock, Trip},
};

pub const DEFAULT_BASE_URL: &str = "https://api.real-debrid.com/rest/1.0";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const PAGE_SIZE: usize = 1000;
const MAX_PAGES: usize = 20;
/// Calls that may go out back to back before the budget starts spacing them.
const BURST: u32 = 4;

/// Everything the worker needs to act at Real-Debrid: the client, the
/// counters file it shares with it, and the limits the run checks.
pub struct RdRuntime {
    pub client: RdClient,
    pub state: StateStore,
    /// The cap on one torrent's file, checked against Real-Debrid's size.
    pub max_bytes: u64,
    /// Active slots left unused for the admin's own downloads.
    pub keep_free_slots: u32,
}

impl RdRuntime {
    pub fn new(config: &RdConfig) -> Result<Self, RdError> {
        let state = StateStore::open_with(
            &config.state_dir,
            Arc::new(SystemClock),
            Limits {
                max_adds: config.max_adds_per_day,
                max_uncached: config.max_uncached_per_day,
            },
        );
        let client = RdClient::new(
            &config.base_url,
            &config.token,
            state.clone(),
            config.per_min,
        )?;
        Ok(Self {
            client,
            state,
            max_bytes: config.max_bytes,
            keep_free_slots: config.keep_free_slots,
        })
    }
}

impl fmt::Debug for RdRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdRuntime")
            .field("client", &self.client)
            .field("max_bytes", &self.max_bytes)
            .field("keep_free_slots", &self.keep_free_slots)
            .finish_non_exhaustive()
    }
}

/// The Real-Debrid API token. Not `Display`, not serializable, and `Debug`
/// shows nothing of it.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[cfg(test)]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    /// The `Authorization` header value, marked sensitive so the HTTP stack
    /// keeps it out of its own debug output.
    fn bearer(&self) -> Result<HeaderValue, RdError> {
        let mut value =
            HeaderValue::from_str(&format!("Bearer {}", self.0)).map_err(|_| RdError::Fatal)?;
        value.set_sensitive(true);
        Ok(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RdError {
    /// Real-Debrid refused this torrent (error 35, infringing file).
    #[error("real-debrid refused the torrent (error {0})")]
    Refused(u32),
    /// The breaker is open, or this response just opened it. Nothing was
    /// sent in the first case.
    #[error("real-debrid is being left alone ({0:?})")]
    Breaker(Trip),
    #[error("not found at real-debrid")]
    NotFound,
    /// Worth trying again later: a server error, a timeout, a lost
    /// connection.
    #[error("real-debrid is temporarily unavailable")]
    Transient,
    /// Anything else: a request we should not have made, an answer we cannot
    /// read.
    #[error("unexpected answer from real-debrid")]
    Fatal,
}

/// The torrent states Real-Debrid documents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TorrentStatus {
    MagnetError,
    MagnetConversion,
    WaitingFilesSelection,
    Queued,
    Downloading,
    Downloaded,
    Error,
    Virus,
    Compressing,
    Uploading,
    Dead,
    Other(String),
}

impl TorrentStatus {
    fn parse(s: &str) -> Self {
        match s {
            "magnet_error" => Self::MagnetError,
            "magnet_conversion" => Self::MagnetConversion,
            "waiting_files_selection" => Self::WaitingFilesSelection,
            "queued" => Self::Queued,
            "downloading" => Self::Downloading,
            "downloaded" => Self::Downloaded,
            "error" => Self::Error,
            "virus" => Self::Virus,
            "compressing" => Self::Compressing,
            "uploading" => Self::Uploading,
            "dead" => Self::Dead,
            other => Self::Other(other.chars().take(40).collect()),
        }
    }
}

impl<'de> Deserialize<'de> for TorrentStatus {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(d)?))
    }
}

/// One entry of the library listing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ListedTorrent {
    pub id: String,
    #[serde(default, deserialize_with = "lowercase")]
    pub hash: String,
    pub status: TorrentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InfoFile {
    pub id: u64,
    pub path: String,
    pub bytes: u64,
    #[serde(deserialize_with = "flag")]
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TorrentInfo {
    pub id: String,
    #[serde(default, deserialize_with = "lowercase")]
    pub hash: String,
    pub status: TorrentStatus,
    /// 0 to 100.
    #[serde(default)]
    pub progress: f64,
    /// Size of the selected files only.
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub files: Vec<InfoFile>,
    #[serde(default)]
    pub links: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct ActiveCount {
    pub nb: u32,
    pub limit: u32,
}

#[derive(Deserialize)]
struct Added {
    id: String,
}

fn lowercase<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(String::deserialize(d)?.to_ascii_lowercase())
}

/// Real-Debrid sends `selected` as 0/1; accept a real boolean too.
fn flag<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Flag {
        Bool(bool),
        Num(i64),
    }
    Ok(match Flag::deserialize(d)? {
        Flag::Bool(b) => b,
        Flag::Num(n) => n != 0,
    })
}

/// A request budget: a few calls may go out together, after that they are
/// spaced evenly so the average stays at `per_min`.
#[derive(Debug)]
pub struct Budget {
    interval: Duration,
    slack: Duration,
    /// When the next call would be due if nothing had been saved up.
    next: Mutex<Option<Instant>>,
}

impl Budget {
    pub fn new(per_min: u32, burst: u32) -> Self {
        let per_min = per_min.max(1);
        let interval = Duration::from_millis(60_000 / u64::from(per_min));
        Self {
            interval,
            slack: interval.saturating_mul(burst.max(1) - 1),
            next: Mutex::new(None),
        }
    }

    /// Waits until a call is allowed. Callers are served in order.
    pub async fn acquire(&self) {
        let mut next = self.next.lock().await;
        let now = Instant::now();
        let due = next.map_or(now, |t| t.max(now));
        let allowed_at = due.checked_sub(self.slack).unwrap_or(now);
        if allowed_at > now {
            tokio::time::sleep_until(allowed_at).await;
        }
        *next = Some(due + self.interval);
    }
}

pub struct RdClient {
    http: reqwest::Client,
    base_url: String,
    state: StateStore,
    budget: Budget,
}

impl fmt::Debug for RdClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl RdClient {
    /// `base_url` must be `https://…`, or `http://127.0.0.1…` for tests.
    pub fn new(
        base_url: &str,
        secret: &Secret,
        state: StateStore,
        per_min: u32,
    ) -> Result<Self, RdError> {
        let url = Url::parse(base_url).map_err(|_| RdError::Fatal)?;
        let local = url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
        if !(url.scheme() == "https" || local)
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(RdError::Fatal);
        }
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, secret.bearer()?);
        let http = reqwest::Client::builder()
            .user_agent(concat!("mediaqueue-worker/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()
            .map_err(|_| RdError::Fatal)?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            state,
            budget: Budget::new(per_min, BURST),
        })
    }

    pub async fn active_count(&self) -> Result<ActiveCount, RdError> {
        let reply = self.execute(self.get("/torrents/activeCount")).await?;
        parse(&reply.body)
    }

    /// The whole library, `PAGE_SIZE` entries per call.
    pub async fn list_torrents(&self) -> Result<Vec<ListedTorrent>, RdError> {
        let mut all: Vec<ListedTorrent> = Vec::new();
        for page in 1..=MAX_PAGES {
            let request = self
                .get("/torrents")
                .query(&[("limit", PAGE_SIZE.to_string()), ("page", page.to_string())]);
            let reply = self.execute(request).await?;
            // An empty library is answered with 204 and no body.
            if reply.status == StatusCode::NO_CONTENT || reply.body.is_empty() {
                return Ok(all);
            }
            let items: Vec<ListedTorrent> = parse(&reply.body)?;
            let got = items.len();
            all.extend(items);
            let done = match reply.total {
                Some(total) => got == 0 || all.len() as u64 >= total,
                None => got < PAGE_SIZE,
            };
            if done {
                return Ok(all);
            }
        }
        // A cut-off listing would make "not in the library" untrue.
        tracing::warn!(pages = MAX_PAGES, "real-debrid library listing too long");
        Err(RdError::Fatal)
    }

    /// Adds a bare magnet link for `hash` and returns the new id. Nothing
    /// but a 40-character hex hash ever reaches the magnet.
    pub async fn add_magnet(&self, hash: &str) -> Result<String, RdError> {
        if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RdError::Fatal);
        }
        let magnet = format!("magnet:?xt=urn:btih:{}", hash.to_ascii_lowercase());
        let request = self
            .http
            .post(self.url("/torrents/addMagnet"))
            .form(&[("magnet", magnet)]);
        let reply = self.execute(request).await?;
        let added: Added = parse(&reply.body)?;
        if !valid_id(&added.id) {
            return Err(RdError::Fatal);
        }
        Ok(added.id)
    }

    pub async fn info(&self, id: &str) -> Result<TorrentInfo, RdError> {
        check_id(id)?;
        let reply = self
            .execute(self.get(&format!("/torrents/info/{id}")))
            .await?;
        parse(&reply.body)
    }

    /// Selects exactly one file. 204 and 202 (already selected) both count.
    pub async fn select_files(&self, id: &str, file_id: u64) -> Result<(), RdError> {
        check_id(id)?;
        let request = self
            .http
            .post(self.url(&format!("/torrents/selectFiles/{id}")))
            .form(&[("files", file_id.to_string())]);
        self.execute(request).await.map(|_| ())
    }

    pub async fn delete(&self, id: &str) -> Result<(), RdError> {
        check_id(id)?;
        let request = self
            .http
            .delete(self.url(&format!("/torrents/delete/{id}")));
        self.execute(request).await.map(|_| ())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn get(&self, path: &str) -> RequestBuilder {
        self.http.request(Method::GET, self.url(path))
    }

    async fn execute(&self, request: RequestBuilder) -> Result<Reply, RdError> {
        self.check_breaker()?;
        self.budget.acquire().await;
        // The breaker may have opened while this call waited for its turn.
        self.check_breaker()?;

        let mut response = request.send().await.map_err(|e| {
            let why = if e.is_timeout() {
                "timeout"
            } else if e.is_connect() {
                "connect failed"
            } else {
                "request failed"
            };
            tracing::info!(why, "real-debrid request failed");
            RdError::Transient
        })?;
        let status = response.status();
        let total = response
            .headers()
            .get("x-total-count")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        if response
            .content_length()
            .is_some_and(|len| len > MAX_BODY_BYTES as u64)
        {
            return Err(RdError::Fatal);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| RdError::Transient)? {
            if body.len() + chunk.len() > MAX_BODY_BYTES {
                return Err(RdError::Fatal);
            }
            body.extend_from_slice(&chunk);
        }

        if status.is_success() {
            self.state.record_success();
            return Ok(Reply {
                status,
                total,
                body,
            });
        }
        let code = error_code(&body);
        tracing::info!(
            "real-debrid refused a call ({})",
            describe(status.as_u16(), code)
        );
        Err(match classify(status.as_u16(), code) {
            Class::Refused(code) => RdError::Refused(code),
            Class::Trip(trip) => {
                if let Err(e) = self.state.trip(trip, self.state.now()) {
                    tracing::warn!(error = %e, "breaker opened but not saved");
                }
                RdError::Breaker(trip)
            }
            Class::NotFound => RdError::NotFound,
            Class::Transient => RdError::Transient,
            Class::Fatal => RdError::Fatal,
        })
    }

    fn check_breaker(&self) -> Result<(), RdError> {
        match self.state.breaker_trip(self.state.now()) {
            Some(trip) => Err(RdError::Breaker(trip)),
            None => Ok(()),
        }
    }
}

struct Reply {
    status: StatusCode,
    total: Option<u64>,
    body: Vec<u8>,
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, RdError> {
    serde_json::from_slice(body).map_err(|_| {
        tracing::warn!("real-debrid answer could not be read");
        RdError::Fatal
    })
}

/// 1 to 64 ASCII alphanumerics, the same rule as the portal's. Checked
/// before an id goes into a URL path.
pub fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn check_id(id: &str) -> Result<(), RdError> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(RdError::Fatal)
    }
}

/// The numeric `error_code` of an error body, if there is one.
fn error_code(body: &[u8]) -> Option<u32> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let code = value.get("error_code")?;
    code.as_u64()
        .or_else(|| code.as_str().and_then(|s| s.parse().ok()))
        .and_then(|n| u32::try_from(n).ok())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Refused(u32),
    Trip(Trip),
    NotFound,
    Transient,
    Fatal,
}

/// The code in the body beats the HTTP status: Real-Debrid has been seen to
/// send the same code under different statuses. The status decides only when
/// there is no code or the code is not one of ours.
fn classify(status: u16, code: Option<u32>) -> Class {
    match code {
        Some(35) => return Class::Refused(35),
        Some(34) => return Class::Trip(Trip::RateLimit),
        Some(9) => return Class::Trip(Trip::Forbidden),
        Some(8) => return Class::Trip(Trip::Auth),
        _ => {}
    }
    match status {
        429 => Class::Trip(Trip::RateLimit),
        403 => Class::Trip(Trip::Forbidden),
        401 => Class::Trip(Trip::Auth),
        404 => Class::NotFound,
        500..=599 => Class::Transient,
        _ => Class::Fatal,
    }
}

/// What the log says about a failed call: two numbers, nothing from the body.
pub fn describe(status: u16, code: Option<u32>) -> String {
    match code {
        Some(code) => format!("status {status}, error_code {code}"),
        None => format!("status {status}, no error_code"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string, header, method, path, query_param},
    };

    use super::*;
    use crate::worker::state::{
        StateStore,
        testutil::{Dir, FakeClock},
    };

    const TOKEN: &str = "test-token-0000";
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ID: &str = "ABCDEFGHIJKLM";
    // 2026-10-20 12:00:00 UTC
    const NOON: u64 = 1_792_497_600;

    struct Rig {
        server: MockServer,
        client: RdClient,
        state: StateStore,
        dir: Dir,
    }

    async fn rig() -> Rig {
        let server = MockServer::start().await;
        let dir = Dir::new();
        let state = StateStore::open(&dir.0, FakeClock::at(NOON));
        let client =
            RdClient::new(&server.uri(), &Secret::new(TOKEN), state.clone(), 6000).unwrap();
        Rig {
            server,
            client,
            state,
            dir,
        }
    }

    fn error_body(code: u32) -> ResponseTemplate {
        ResponseTemplate::new(403).set_body_json(json!({ "error": "x", "error_code": code }))
    }

    async fn mount_status(rig: &Rig, template: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/torrents/activeCount"))
            .respond_with(template)
            .mount(&rig.server)
            .await;
    }

    async fn requests(rig: &Rig) -> usize {
        rig.server.received_requests().await.unwrap().len()
    }

    fn info_json() -> serde_json::Value {
        json!({
            "id": ID,
            "filename": "Example.Movie.1994.1080p.WEB-DL.AAC.mkv",
            "hash": HASH.to_uppercase(),
            "bytes": 4_000_000_000u64,
            "original_bytes": 4_100_000_000u64,
            "progress": 100,
            "status": "downloaded",
            "files": [
                { "id": 1, "path": "/Example.Movie.1994.1080p.WEB-DL.AAC.mkv", "bytes": 4_000_000_000u64, "selected": 1 },
                { "id": 2, "path": "/info.nfo", "bytes": 1200, "selected": 0 },
            ],
            "links": ["https://example.invalid/d/1"],
            "speed": 0,
            "seeders": 12,
            "added": "2026-10-20T12:00:00.000Z",
        })
    }

    #[tokio::test]
    async fn active_count_sends_the_bearer_header_and_reads_the_answer() {
        let rig = rig().await;
        Mock::given(method("GET"))
            .and(path("/torrents/activeCount"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "nb": 3, "limit": 25 })))
            .expect(1)
            .mount(&rig.server)
            .await;
        let count = rig.client.active_count().await.unwrap();
        assert_eq!(count, ActiveCount { nb: 3, limit: 25 });
    }

    #[tokio::test]
    async fn add_magnet_posts_a_bare_magnet_and_returns_the_id() {
        let rig = rig().await;
        let hash = HASH.to_uppercase();
        Mock::given(method("POST"))
            .and(path("/torrents/addMagnet"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .and(body_string(format!(
                "magnet=magnet%3A%3Fxt%3Durn%3Abtih%3A{HASH}"
            )))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({ "id": ID, "uri": "https://example.invalid/t/1" })),
            )
            .expect(1)
            .mount(&rig.server)
            .await;
        assert_eq!(rig.client.add_magnet(&hash).await.unwrap(), ID);
    }

    #[tokio::test]
    async fn add_magnet_refuses_anything_but_forty_hex_characters_without_a_request() {
        let rig = rig().await;
        for bad in [
            "",
            "aaaa",
            &"g".repeat(40),
            &"a".repeat(41),
            &format!("{HASH}&dn=x"),
            &format!("{}\n", &HASH[..39]),
        ] {
            assert_eq!(
                rig.client.add_magnet(bad).await,
                Err(RdError::Fatal),
                "{bad:?}"
            );
        }
        assert_eq!(requests(&rig).await, 0);
    }

    #[tokio::test]
    async fn add_magnet_rejects_a_bad_id_in_the_answer() {
        let rig = rig().await;
        Mock::given(method("POST"))
            .and(path("/torrents/addMagnet"))
            .respond_with(
                ResponseTemplate::new(201).set_body_json(json!({ "id": "../x", "uri": "u" })),
            )
            .mount(&rig.server)
            .await;
        assert_eq!(rig.client.add_magnet(HASH).await, Err(RdError::Fatal));
    }

    #[tokio::test]
    async fn info_reads_files_flags_status_and_lowercases_the_hash() {
        let rig = rig().await;
        Mock::given(method("GET"))
            .and(path(format!("/torrents/info/{ID}")))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(info_json()))
            .expect(1)
            .mount(&rig.server)
            .await;
        let info = rig.client.info(ID).await.unwrap();
        assert_eq!(info.id, ID);
        assert_eq!(info.hash, HASH);
        assert_eq!(info.status, TorrentStatus::Downloaded);
        assert_eq!(info.progress, 100.0);
        assert_eq!(info.bytes, 4_000_000_000);
        assert_eq!(info.files.len(), 2);
        assert!(info.files[0].selected && !info.files[1].selected);
        assert_eq!(info.links.len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_status_is_kept_as_other() {
        let rig = rig().await;
        let mut body = info_json();
        body["status"] = json!("brand_new_state");
        Mock::given(method("GET"))
            .and(path(format!("/torrents/info/{ID}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&rig.server)
            .await;
        let info = rig.client.info(ID).await.unwrap();
        assert_eq!(info.status, TorrentStatus::Other("brand_new_state".into()));
    }

    #[tokio::test]
    async fn every_documented_status_parses() {
        for (text, status) in [
            ("magnet_error", TorrentStatus::MagnetError),
            ("magnet_conversion", TorrentStatus::MagnetConversion),
            (
                "waiting_files_selection",
                TorrentStatus::WaitingFilesSelection,
            ),
            ("queued", TorrentStatus::Queued),
            ("downloading", TorrentStatus::Downloading),
            ("downloaded", TorrentStatus::Downloaded),
            ("error", TorrentStatus::Error),
            ("virus", TorrentStatus::Virus),
            ("compressing", TorrentStatus::Compressing),
            ("uploading", TorrentStatus::Uploading),
            ("dead", TorrentStatus::Dead),
        ] {
            assert_eq!(TorrentStatus::parse(text), status);
        }
    }

    #[tokio::test]
    async fn select_files_posts_one_file_id_and_accepts_204_and_202() {
        for status in [204, 202] {
            let rig = rig().await;
            Mock::given(method("POST"))
                .and(path(format!("/torrents/selectFiles/{ID}")))
                .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
                .and(body_string("files=7"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&rig.server)
                .await;
            assert_eq!(rig.client.select_files(ID, 7).await, Ok(()), "{status}");
        }
    }

    #[tokio::test]
    async fn delete_uses_delete_and_treats_404_as_not_found() {
        let rig = rig().await;
        Mock::given(method("DELETE"))
            .and(path(format!("/torrents/delete/{ID}")))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&rig.server)
            .await;
        assert_eq!(rig.client.delete(ID).await, Ok(()));

        let rig = self::rig().await;
        Mock::given(method("DELETE"))
            .respond_with(
                ResponseTemplate::new(404)
                    .set_body_json(json!({ "error": "unknown_ressource", "error_code": 7 })),
            )
            .mount(&rig.server)
            .await;
        assert_eq!(rig.client.delete(ID).await, Err(RdError::NotFound));
    }

    #[tokio::test]
    async fn info_and_select_404_are_not_found() {
        let rig = rig().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&rig.server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&rig.server)
            .await;
        assert_eq!(rig.client.info(ID).await.unwrap_err(), RdError::NotFound);
        assert_eq!(rig.client.select_files(ID, 1).await, Err(RdError::NotFound));
    }

    #[tokio::test]
    async fn ids_that_could_change_the_path_are_refused_without_a_request() {
        let rig = rig().await;
        for bad in ["", "../x", "a/b", "a b", "x?y=1", &"A".repeat(65)] {
            assert_eq!(
                rig.client.info(bad).await.unwrap_err(),
                RdError::Fatal,
                "{bad:?}"
            );
            assert_eq!(rig.client.select_files(bad, 1).await, Err(RdError::Fatal));
            assert_eq!(rig.client.delete(bad).await, Err(RdError::Fatal));
        }
        assert_eq!(requests(&rig).await, 0);
    }

    fn listed(from: usize, count: usize) -> serde_json::Value {
        let items: Vec<_> = (from..from + count)
            .map(|n| json!({ "id": format!("ID{n}"), "hash": format!("{n:040x}"), "status": "downloaded", "filename": "x" }))
            .collect();
        json!(items)
    }

    #[tokio::test]
    async fn list_torrents_reads_pages_until_the_total() {
        let rig = rig().await;
        Mock::given(method("GET"))
            .and(path("/torrents"))
            .and(query_param("limit", "1000"))
            .and(query_param("page", "1"))
            .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "1500")
                    .set_body_json(listed(0, 1000)),
            )
            .expect(1)
            .mount(&rig.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/torrents"))
            .and(query_param("page", "2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "1500")
                    .set_body_json(listed(1000, 500)),
            )
            .expect(1)
            .mount(&rig.server)
            .await;
        let all = rig.client.list_torrents().await.unwrap();
        assert_eq!(all.len(), 1500);
        assert_eq!(all[1499].id, "ID1499");
        assert_eq!(all[3].hash, format!("{:040x}", 3));
        assert_eq!(requests(&rig).await, 2);
    }

    #[tokio::test]
    async fn list_torrents_handles_an_empty_library_and_a_missing_total() {
        let rig = rig().await;
        Mock::given(method("GET"))
            .and(path("/torrents"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&rig.server)
            .await;
        assert!(rig.client.list_torrents().await.unwrap().is_empty());

        let rig = self::rig().await;
        Mock::given(method("GET"))
            .and(path("/torrents"))
            .respond_with(ResponseTemplate::new(200).set_body_json(listed(0, 3)))
            .expect(1)
            .mount(&rig.server)
            .await;
        assert_eq!(rig.client.list_torrents().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_library_that_never_ends_is_an_error_not_a_short_list() {
        let rig = rig().await;
        Mock::given(method("GET"))
            .and(path("/torrents"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Total-Count", "999999")
                    .set_body_json(listed(0, 1)),
            )
            .mount(&rig.server)
            .await;
        assert_eq!(
            rig.client.list_torrents().await.unwrap_err(),
            RdError::Fatal
        );
    }

    #[test]
    fn classification_table() {
        let t = |s, c| classify(s, c);
        // The code beats the status.
        assert_eq!(t(403, Some(35)), Class::Refused(35));
        assert_eq!(t(400, Some(35)), Class::Refused(35));
        assert_eq!(t(200, Some(35)), Class::Refused(35));
        assert_eq!(t(429, Some(34)), Class::Trip(Trip::RateLimit));
        assert_eq!(t(403, Some(34)), Class::Trip(Trip::RateLimit));
        assert_eq!(t(403, Some(9)), Class::Trip(Trip::Forbidden));
        assert_eq!(t(401, Some(9)), Class::Trip(Trip::Forbidden));
        assert_eq!(t(403, Some(8)), Class::Trip(Trip::Auth));
        assert_eq!(t(401, Some(8)), Class::Trip(Trip::Auth));
        assert_eq!(t(404, Some(35)), Class::Refused(35));
        // No code, or a code we do not name: the status decides.
        assert_eq!(t(429, None), Class::Trip(Trip::RateLimit));
        assert_eq!(t(403, None), Class::Trip(Trip::Forbidden));
        assert_eq!(t(401, None), Class::Trip(Trip::Auth));
        assert_eq!(t(404, None), Class::NotFound);
        assert_eq!(t(404, Some(7)), Class::NotFound);
        assert_eq!(t(500, None), Class::Transient);
        assert_eq!(t(503, Some(21)), Class::Transient);
        assert_eq!(t(502, None), Class::Transient);
        assert_eq!(t(400, None), Class::Fatal);
        assert_eq!(t(400, Some(1)), Class::Fatal);
        assert_eq!(t(302, None), Class::Fatal);
    }

    #[test]
    fn error_codes_are_read_from_numbers_and_numeric_strings_only() {
        assert_eq!(error_code(br#"{"error":"x","error_code":35}"#), Some(35));
        assert_eq!(error_code(br#"{"error_code":"34"}"#), Some(34));
        assert_eq!(error_code(br#"{"error_code":-1}"#), None);
        assert_eq!(error_code(br#"{"error_code":"abc"}"#), None);
        assert_eq!(error_code(br#"{"error":"x"}"#), None);
        assert_eq!(error_code(b"<html>forbidden</html>"), None);
        assert_eq!(error_code(b""), None);
    }

    #[test]
    fn describe_has_two_numbers_and_nothing_else() {
        assert_eq!(describe(403, Some(35)), "status 403, error_code 35");
        assert_eq!(describe(429, None), "status 429, no error_code");
    }

    #[tokio::test]
    async fn the_code_beats_the_status_on_the_wire() {
        let cases: [(u16, Option<u32>, RdError); 6] = [
            (403, Some(35), RdError::Refused(35)),
            (400, Some(35), RdError::Refused(35)),
            (403, None, RdError::Breaker(Trip::Forbidden)),
            (429, Some(34), RdError::Breaker(Trip::RateLimit)),
            (403, Some(8), RdError::Breaker(Trip::Auth)),
            (429, None, RdError::Breaker(Trip::RateLimit)),
        ];
        for (status, code, expected) in cases {
            let rig = rig().await;
            let template = ResponseTemplate::new(status);
            let template = match code {
                Some(c) => template.set_body_json(json!({ "error": "x", "error_code": c })),
                None => template,
            };
            mount_status(&rig, template).await;
            assert_eq!(
                rig.client.active_count().await.unwrap_err(),
                expected,
                "{status} {code:?}"
            );
        }
        let rig = rig().await;
        mount_status(&rig, ResponseTemplate::new(401)).await;
        assert_eq!(
            rig.client.active_count().await.unwrap_err(),
            RdError::Breaker(Trip::Auth)
        );
    }

    #[tokio::test]
    async fn a_refusal_does_not_open_the_breaker() {
        let rig = rig().await;
        mount_status(&rig, error_body(35)).await;
        assert_eq!(
            rig.client.active_count().await.unwrap_err(),
            RdError::Refused(35)
        );
        assert_eq!(rig.state.breaker_trip(NOON), None);
        assert_eq!(
            rig.client.active_count().await.unwrap_err(),
            RdError::Refused(35)
        );
        assert_eq!(requests(&rig).await, 2);
    }

    #[tokio::test]
    async fn server_errors_are_transient_and_other_client_errors_fatal() {
        for (status, expected) in [
            (500, RdError::Transient),
            (503, RdError::Transient),
            (400, RdError::Fatal),
            (418, RdError::Fatal),
        ] {
            let rig = rig().await;
            mount_status(&rig, ResponseTemplate::new(status)).await;
            assert_eq!(rig.client.active_count().await.unwrap_err(), expected);
            assert_eq!(rig.state.breaker_trip(NOON), None);
        }
    }

    #[tokio::test]
    async fn a_tripping_response_is_saved_and_blocks_later_calls_without_a_request() {
        let rig = rig().await;
        mount_status(
            &rig,
            ResponseTemplate::new(429).set_body_json(json!({ "error_code": 34 })),
        )
        .await;
        assert_eq!(
            rig.client.active_count().await.unwrap_err(),
            RdError::Breaker(Trip::RateLimit)
        );
        assert_eq!(requests(&rig).await, 1);

        // Saved before the call returned.
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(rig.dir.file()).unwrap()).unwrap();
        assert_eq!(saved["breaker_until"], NOON + 900);

        assert_eq!(
            rig.client.active_count().await.unwrap_err(),
            RdError::Breaker(Trip::RateLimit)
        );
        assert_eq!(
            rig.client.info(ID).await.unwrap_err(),
            RdError::Breaker(Trip::RateLimit)
        );
        assert_eq!(
            rig.client.add_magnet(HASH).await,
            Err(RdError::Breaker(Trip::RateLimit))
        );
        assert_eq!(
            rig.client.delete(ID).await,
            Err(RdError::Breaker(Trip::RateLimit))
        );
        assert_eq!(
            rig.client.list_torrents().await.unwrap_err(),
            RdError::Breaker(Trip::RateLimit)
        );
        assert_eq!(requests(&rig).await, 1);
    }

    #[tokio::test]
    async fn a_bad_token_closes_the_client_until_restart() {
        let rig = rig().await;
        mount_status(
            &rig,
            ResponseTemplate::new(401).set_body_json(json!({ "error_code": 8 })),
        )
        .await;
        assert_eq!(
            rig.client.active_count().await.unwrap_err(),
            RdError::Breaker(Trip::Auth)
        );
        assert_eq!(
            rig.client.info(ID).await.unwrap_err(),
            RdError::Breaker(Trip::Auth)
        );
        assert_eq!(requests(&rig).await, 1);
    }

    #[tokio::test]
    async fn a_success_resets_the_rate_limit_count() {
        let rig = rig().await;
        rig.state.trip(Trip::RateLimit, 1).unwrap();
        assert_eq!(rig.state.breaker_trip(NOON), None);
        Mock::given(method("GET"))
            .and(path("/torrents/activeCount"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "nb": 0, "limit": 25 })))
            .mount(&rig.server)
            .await;
        rig.client.active_count().await.unwrap();
        rig.state.trip(Trip::RateLimit, NOON).unwrap();
        assert_eq!(rig.state.breaker_open(NOON), Some(Duration::from_secs(900)));
    }

    #[tokio::test(start_paused = true)]
    async fn the_budget_lets_a_burst_through_then_spaces_calls() {
        let budget = Budget::new(20, 4);
        let start = Instant::now();
        for _ in 0..4 {
            budget.acquire().await;
        }
        assert_eq!(start.elapsed(), Duration::ZERO);
        for n in 1..=3u32 {
            budget.acquire().await;
            assert_eq!(start.elapsed(), Duration::from_secs(3) * n);
        }
        // Idle time is saved up, but never more than the burst.
        tokio::time::sleep(Duration::from_secs(600)).await;
        let idle_end = Instant::now();
        for _ in 0..4 {
            budget.acquire().await;
        }
        assert_eq!(idle_end.elapsed(), Duration::ZERO);
        budget.acquire().await;
        assert_eq!(idle_end.elapsed(), Duration::from_secs(3));
    }

    #[tokio::test]
    async fn the_client_spaces_its_calls() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/torrents/activeCount"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "nb": 0, "limit": 25 })))
            .mount(&server)
            .await;
        let dir = Dir::new();
        let state = StateStore::open(&dir.0, FakeClock::at(NOON));
        // 600 a minute is one every 100 ms after the burst of four.
        let client = RdClient::new(&server.uri(), &Secret::new(TOKEN), state, 600).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..7 {
            client.active_count().await.unwrap();
        }
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "{:?}",
            start.elapsed()
        );
    }

    #[test]
    fn the_token_is_in_no_debug_output_and_no_error_text() {
        let secret = Secret::new(TOKEN);
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{secret:#?}"), "Secret(<redacted>)");

        let dir = Dir::new();
        let state = StateStore::open(&dir.0, FakeClock::at(NOON));
        let client = RdClient::new(DEFAULT_BASE_URL, &secret, state, 20).unwrap();
        assert!(!format!("{client:?}").contains(TOKEN));
        assert!(!format!("{client:#?}").contains("test-token"));

        for error in [
            RdError::Refused(35),
            RdError::Breaker(Trip::Auth),
            RdError::NotFound,
            RdError::Transient,
            RdError::Fatal,
        ] {
            assert!(!format!("{error} {error:?}").contains("test-token"));
        }
    }

    #[tokio::test]
    async fn errors_from_a_real_exchange_do_not_carry_the_token_either() {
        let rig = rig().await;
        // The server echoes the token back in its error body.
        mount_status(
            &rig,
            ResponseTemplate::new(400).set_body_string(format!("bad request for {TOKEN}")),
        )
        .await;
        let error = rig.client.active_count().await.unwrap_err();
        assert!(!format!("{error} {error:?}").contains("test-token"));
    }

    #[test]
    fn the_authorization_header_value_is_marked_sensitive() {
        let value = Secret::new(TOKEN).bearer().unwrap();
        assert!(value.is_sensitive());
        assert!(!format!("{value:?}").contains("test-token"));
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed() {
        let rig = rig().await;
        Mock::given(method("GET"))
            .and(path("/torrents/activeCount"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/elsewhere"))
            .mount(&rig.server)
            .await;
        Mock::given(path("/elsewhere"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "nb": 0, "limit": 1 })))
            .expect(0)
            .mount(&rig.server)
            .await;
        assert_eq!(rig.client.active_count().await.unwrap_err(), RdError::Fatal);
        assert_eq!(requests(&rig).await, 1);
    }

    #[tokio::test]
    async fn an_oversized_or_unreadable_answer_is_fatal() {
        let rig = rig().await;
        mount_status(
            &rig,
            ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_BODY_BYTES + 1]),
        )
        .await;
        assert_eq!(rig.client.active_count().await.unwrap_err(), RdError::Fatal);

        let rig = self::rig().await;
        mount_status(&rig, ResponseTemplate::new(200).set_body_string("<html>")).await;
        assert_eq!(rig.client.active_count().await.unwrap_err(), RdError::Fatal);
    }

    #[test]
    fn the_base_url_must_be_https_or_loopback_http() {
        let dir = Dir::new();
        let state = StateStore::open(&dir.0, FakeClock::at(NOON));
        let secret = Secret::new(TOKEN);
        let make = |url: &str| RdClient::new(url, &secret, state.clone(), 20).map(|_| ());
        assert!(make("https://api.real-debrid.com/rest/1.0").is_ok());
        assert!(make("http://127.0.0.1:8080/rest/1.0").is_ok());
        for bad in [
            "http://api.real-debrid.com/rest/1.0",
            "http://127.0.0.1.example.com/",
            "http://localhost:8080",
            "ftp://127.0.0.1",
            "https://user:pw@api.real-debrid.com/",
            "not a url",
            "",
        ] {
            assert_eq!(make(bad), Err(RdError::Fatal), "{bad}");
        }
    }

    #[test]
    fn a_header_unsafe_token_is_refused_without_echoing_it() {
        let dir = Dir::new();
        let state = StateStore::open(&dir.0, FakeClock::at(NOON));
        let error =
            RdClient::new(DEFAULT_BASE_URL, &Secret::new("bad\ntoken"), state, 20).unwrap_err();
        assert_eq!(error, RdError::Fatal);
        assert!(!format!("{error}").contains("bad"));
    }
}
