//! The worker's client for the portal's internal API (`/internal/...`).
//!
//! Every call carries the bearer token. Nothing here logs the token or a
//! response body; the only thing taken from a body is the portal's `error`
//! code.

use std::{fmt, time::Duration};

use reqwest::{Response, StatusCode, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The portal's answers are small; anything bigger is not the portal.
const MAX_BODY_BYTES: usize = 256 * 1024;

/// The longest `error` text the portal accepts is 500 characters; stay under.
const MAX_ERROR_CHARS: usize = 400;

/// The worker's mode as the portal reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    DryRun,
    Assisted,
    Auto,
}

/// `GET /internal/settings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct Settings {
    pub enabled: bool,
    pub mode: Mode,
}

/// A claimed job: the fields of the portal's `ClaimView`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Job {
    pub id: i64,
    pub claim_id: String,
    pub tmdb_id: i64,
    pub imdb_id: String,
    /// `movie` or `tv`; a `tv` job can exist and the worker fails it.
    pub media_type: String,
    pub title: String,
    pub year: Option<i64>,
    pub infuse_pro: bool,
    pub chosen_infohash: Option<String>,
    /// A human chose `chosen_infohash`: never fall back to another one.
    pub picked: bool,
    pub rd_torrent_id: Option<String>,
    /// A torrent the admin wants gone; the worker deletes it only if it
    /// created it itself.
    #[serde(default)]
    pub remove_rd_torrent_id: Option<String>,
    pub blocked_hashes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportState {
    NeedsPick,
    Failed,
    Done,
}

/// The body of `POST /internal/jobs/{id}/report`, minus the `claim_id`,
/// which `PortalClient::report` adds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub state: ReportState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<Map<String, Value>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_infohash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chosen_infohash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rd_torrent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Report {
    fn new(state: ReportState) -> Self {
        Self {
            state,
            candidates: None,
            top_infohash: None,
            chosen_infohash: None,
            rd_torrent_id: None,
            reason: None,
            error: None,
        }
    }

    pub fn needs_pick(
        candidates: Vec<Map<String, Value>>,
        top_infohash: Option<String>,
        reason: &'static str,
    ) -> Self {
        Self {
            candidates: Some(candidates),
            top_infohash,
            reason: Some(reason),
            ..Self::new(ReportState::NeedsPick)
        }
    }

    pub fn failed(reason: Option<&'static str>, error: &str) -> Self {
        Self {
            reason,
            error: Some(truncate_error(error)),
            ..Self::new(ReportState::Failed)
        }
    }

    /// The release is at Real-Debrid and verified: the request is added.
    pub fn done(
        rd_torrent_id: String,
        chosen_infohash: String,
        top_infohash: Option<String>,
    ) -> Self {
        Self {
            rd_torrent_id: Some(rd_torrent_id),
            chosen_infohash: Some(chosen_infohash),
            top_infohash,
            ..Self::new(ReportState::Done)
        }
    }

    /// The release was already in the library: nothing added, no torrent id.
    pub fn already_had(chosen_infohash: String) -> Self {
        Self {
            chosen_infohash: Some(chosen_infohash),
            reason: Some("already_had"),
            ..Self::new(ReportState::Done)
        }
    }

    /// Back to the admin with a reason and no new shortlist (the portal
    /// keeps the one it holds).
    pub fn needs_pick_with(reason: &'static str, error: &str) -> Self {
        Self {
            reason: Some(reason),
            error: Some(truncate_error(error)),
            ..Self::new(ReportState::NeedsPick)
        }
    }
}

/// The body of `POST /internal/jobs/{id}/progress`, minus the `claim_id`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Progress {
    /// Sent the moment Real-Debrid hands out an id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rd_torrent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chosen_infohash: Option<String>,
    /// The worker deleted this torrent for a removal request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_rd_torrent_id: Option<String>,
    /// Add-and-delete cycles since the last call (0 to 5).
    #[serde(skip_serializing_if = "is_zero")]
    pub uncached_adds: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressResult {
    Applied,
    Abandon(Abandon),
}

/// The portal rejects an `error` over 500 characters instead of cutting it.
fn truncate_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

/// Why a report needs no clean-up and no retry: the job is no longer this
/// worker's. In dry-run nothing was added anywhere, so the run is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Abandon {
    /// 409: another claim holds the job, or it was settled. The portal names
    /// the stored torrent only when it has one.
    StaleClaim { rd_torrent_id: Option<String> },
    /// 404: the job is gone.
    NotFound,
    /// 400 `picked_locked`: a human chose a hash this report did not name.
    PickedLocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportResult {
    Applied,
    Abandon(Abandon),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PortalError {
    /// Network error, 5xx or 408: worth trying again.
    #[error("portal call failed, may be retried: {0}")]
    Transient(String),
    /// 401, another 4xx or an answer that makes no sense: retrying changes
    /// nothing.
    #[error("portal call failed: {0}")]
    Fatal(String),
}

#[derive(Clone)]
pub struct PortalClient {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

impl fmt::Debug for PortalClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortalClient")
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// What the status of an answer means, before its body is read.
enum Answer {
    Ok,
    NoContent,
    Failed(StatusCode),
}

#[derive(Serialize)]
struct ReportRequest<'a> {
    claim_id: &'a str,
    #[serde(flatten)]
    report: &'a Report,
}

#[derive(Serialize)]
struct ProgressRequest<'a> {
    claim_id: &'a str,
    #[serde(flatten)]
    progress: &'a Progress,
}

impl PortalClient {
    pub fn new(base_url: &str, token: &str) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("mediaqueue-worker/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(Policy::none())
            .build()?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    pub async fn settings(&self) -> Result<Settings, PortalError> {
        let response = self
            .send(self.client.get(self.url("/internal/settings")))
            .await?;
        match answer(&response) {
            Answer::Ok => read_json(response).await,
            Answer::NoContent => Err(PortalError::Fatal("unexpected 204".into())),
            Answer::Failed(status) => Err(failure(status, response).await),
        }
    }

    /// `None` when there is nothing to do, or the kill switch is off.
    pub async fn claim(&self) -> Result<Option<Job>, PortalError> {
        let response = self
            .send(self.client.post(self.url("/internal/jobs/claim")))
            .await?;
        match answer(&response) {
            Answer::Ok => read_json(response).await.map(Some),
            Answer::NoContent => Ok(None),
            Answer::Failed(status) => Err(failure(status, response).await),
        }
    }

    pub async fn report(
        &self,
        job_id: i64,
        claim_id: &str,
        report: &Report,
    ) -> Result<ReportResult, PortalError> {
        let request = self
            .client
            .post(self.url(&format!("/internal/jobs/{job_id}/report")))
            .json(&ReportRequest { claim_id, report });
        let response = self.send(request).await?;
        Ok(match settle(response).await? {
            None => ReportResult::Applied,
            Some(why) => ReportResult::Abandon(why),
        })
    }

    /// A sign of life that also saves a torrent id, a hash or a finished
    /// removal. It refreshes the claim, so a long wait is not reclaimed; a
    /// 409 here is how the worker learns the admin retried, cancelled or
    /// picked meanwhile.
    pub async fn progress(
        &self,
        job_id: i64,
        claim_id: &str,
        progress: &Progress,
    ) -> Result<ProgressResult, PortalError> {
        let request = self
            .client
            .post(self.url(&format!("/internal/jobs/{job_id}/progress")))
            .json(&ProgressRequest { claim_id, progress });
        let response = self.send(request).await?;
        Ok(match settle(response).await? {
            None => ProgressResult::Applied,
            Some(why) => ProgressResult::Abandon(why),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Response, PortalError> {
        request
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| PortalError::Transient(network_reason(&e).into()))
    }
}

/// What a report or progress answer means: `None` when it was applied,
/// otherwise why the run is no longer this worker's.
async fn settle(response: Response) -> Result<Option<Abandon>, PortalError> {
    match answer(&response) {
        Answer::Ok | Answer::NoContent => Ok(None),
        Answer::Failed(StatusCode::NOT_FOUND) => Ok(Some(Abandon::NotFound)),
        Answer::Failed(StatusCode::CONFLICT) => {
            let body = read_body(response).await.unwrap_or_default();
            Ok(Some(Abandon::StaleClaim {
                rd_torrent_id: stored_torrent(&body),
            }))
        }
        Answer::Failed(StatusCode::BAD_REQUEST) => {
            let body = read_body(response).await.unwrap_or_default();
            match error_code(&body) {
                Some(code) if code == "picked_locked" => Ok(Some(Abandon::PickedLocked)),
                code => Err(PortalError::Fatal(describe(StatusCode::BAD_REQUEST, code))),
            }
        }
        Answer::Failed(status) => Err(failure(status, response).await),
    }
}

fn answer(response: &Response) -> Answer {
    match response.status() {
        StatusCode::OK => Answer::Ok,
        StatusCode::NO_CONTENT => Answer::NoContent,
        other => Answer::Failed(other),
    }
}

/// A short reason with no URL in it.
fn network_reason(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect failed"
    } else {
        "request failed"
    }
}

/// The error for a failed status: 5xx and 408 may be retried, the rest may
/// not. Only the portal's `error` code is taken from the body.
async fn failure(status: StatusCode, response: Response) -> PortalError {
    let body = read_body(response).await.unwrap_or_default();
    let text = describe(status, error_code(&body));
    if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
        PortalError::Transient(text)
    } else {
        PortalError::Fatal(text)
    }
}

fn describe(status: StatusCode, code: Option<String>) -> String {
    match code {
        Some(code) => format!("status {} ({code})", status.as_u16()),
        None => format!("status {}", status.as_u16()),
    }
}

/// The body, read to at most `MAX_BODY_BYTES`.
async fn read_body(mut response: Response) -> Result<Vec<u8>, PortalError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| PortalError::Transient(network_reason(&e).into()))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(PortalError::Fatal("response too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn read_json<T: for<'de> Deserialize<'de>>(response: Response) -> Result<T, PortalError> {
    let body = read_body(response).await?;
    serde_json::from_slice(&body).map_err(|_| PortalError::Fatal("unreadable response".into()))
}

/// The portal's `error` code, if the body is its error shape. Anything else
/// in the body is ignored.
fn error_code(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let code = value.get("error")?.as_str()?;
    let plain = code.len() <= 64 && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    plain.then(|| code.to_string())
}

fn stored_torrent(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value.get("rd_torrent_id")?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{body_json, header, method, path},
    };

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn claim_json() -> Value {
        json!({
            "id": 7,
            "claim_id": "c-1",
            "tmdb_id": 603,
            "imdb_id": "tt1234567",
            "media_type": "movie",
            "title": "Example Movie",
            "year": 1994,
            "infuse_pro": false,
            "chosen_infohash": null,
            "picked": false,
            "rd_torrent_id": null,
            "blocked_hashes": ["0000000000000000000000000000000000000001"],
        })
    }

    async fn client_for(server: &MockServer) -> PortalClient {
        PortalClient::new(&server.uri(), TOKEN).unwrap()
    }

    async fn mount(server: &MockServer, verb: &str, route: &str, template: ResponseTemplate) {
        Mock::given(method(verb))
            .and(path(route))
            .respond_with(template)
            .mount(server)
            .await;
    }

    fn sample_report() -> Report {
        Report::needs_pick(vec![Map::new()], Some("a".repeat(40)), "dry_run")
    }

    async fn report_status(template: ResponseTemplate) -> Result<ReportResult, PortalError> {
        let server = MockServer::start().await;
        mount(&server, "POST", "/internal/jobs/7/report", template).await;
        client_for(&server)
            .await
            .report(7, "c-1", &sample_report())
            .await
    }

    #[tokio::test]
    async fn settings_are_read() {
        let server = MockServer::start().await;
        mount(
            &server,
            "GET",
            "/internal/settings",
            ResponseTemplate::new(200).set_body_json(json!({"enabled": true, "mode": "dry_run"})),
        )
        .await;
        let settings = client_for(&server).await.settings().await.unwrap();
        assert_eq!(
            settings,
            Settings {
                enabled: true,
                mode: Mode::DryRun
            }
        );
    }

    #[tokio::test]
    async fn every_mode_is_understood_and_an_unknown_one_is_fatal() {
        for (text, mode) in [
            ("dry_run", Mode::DryRun),
            ("assisted", Mode::Assisted),
            ("auto", Mode::Auto),
        ] {
            let server = MockServer::start().await;
            mount(
                &server,
                "GET",
                "/internal/settings",
                ResponseTemplate::new(200).set_body_json(json!({"enabled": false, "mode": text})),
            )
            .await;
            assert_eq!(
                client_for(&server).await.settings().await.unwrap().mode,
                mode
            );
        }
        let server = MockServer::start().await;
        mount(
            &server,
            "GET",
            "/internal/settings",
            ResponseTemplate::new(200).set_body_json(json!({"enabled": true, "mode": "yolo"})),
        )
        .await;
        assert!(matches!(
            client_for(&server).await.settings().await,
            Err(PortalError::Fatal(_))
        ));
    }

    #[tokio::test]
    async fn a_claim_reads_into_a_job() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(200).set_body_json(claim_json()),
        )
        .await;
        let job = client_for(&server).await.claim().await.unwrap().unwrap();
        assert_eq!(job.id, 7);
        assert_eq!(job.claim_id, "c-1");
        assert_eq!(job.imdb_id, "tt1234567");
        assert_eq!(job.year, Some(1994));
        assert!(!job.picked && !job.infuse_pro);
        assert_eq!(job.chosen_infohash, None);
        assert_eq!(job.blocked_hashes.len(), 1);
    }

    #[tokio::test]
    async fn a_claim_tolerates_a_field_the_worker_does_not_know() {
        let mut body = claim_json();
        body["something_new"] = json!(1);
        body["year"] = json!(null);
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;
        let job = client_for(&server).await.claim().await.unwrap().unwrap();
        assert_eq!(job.year, None);
    }

    #[tokio::test]
    async fn no_content_on_a_claim_is_no_job() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(204),
        )
        .await;
        assert_eq!(client_for(&server).await.claim().await, Ok(None));
    }

    #[tokio::test]
    async fn every_call_carries_the_bearer_token() {
        let server = MockServer::start().await;
        let bearer = format!("Bearer {TOKEN}");
        for (verb, route, template) in [
            (
                "GET",
                "/internal/settings",
                ResponseTemplate::new(200).set_body_json(json!({"enabled": true, "mode": "auto"})),
            ),
            ("POST", "/internal/jobs/claim", ResponseTemplate::new(204)),
            (
                "POST",
                "/internal/jobs/7/report",
                ResponseTemplate::new(200).set_body_json(json!({"ok": true})),
            ),
        ] {
            Mock::given(method(verb))
                .and(path(route))
                .and(header("authorization", bearer.as_str()))
                .respond_with(template)
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = client_for(&server).await;
        client.settings().await.unwrap();
        client.claim().await.unwrap();
        client.report(7, "c-1", &sample_report()).await.unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn the_report_body_has_the_portals_field_names() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/jobs/7/report"))
            .and(body_json(json!({
                "claim_id": "c-1",
                "state": "needs_pick",
                "candidates": [{"infohash": "a".repeat(40), "score": 71.5}],
                "top_infohash": "a".repeat(40),
                "reason": "dry_run",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let candidate = json!({"infohash": "a".repeat(40), "score": 71.5});
        let report = Report::needs_pick(
            vec![candidate.as_object().unwrap().clone()],
            Some("a".repeat(40)),
            "dry_run",
        );
        let result = client_for(&server).await.report(7, "c-1", &report).await;
        assert_eq!(result, Ok(ReportResult::Applied));
        server.verify().await;
    }

    #[tokio::test]
    async fn a_claim_carries_the_removal_and_defaults_to_none() {
        let server = MockServer::start().await;
        let mut body = claim_json();
        body["remove_rd_torrent_id"] = json!("ABCDEFGHIJKLM");
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(200).set_body_json(body),
        )
        .await;
        let job = client_for(&server).await.claim().await.unwrap().unwrap();
        assert_eq!(job.remove_rd_torrent_id.as_deref(), Some("ABCDEFGHIJKLM"));

        // An older portal that does not send the field.
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(200).set_body_json(claim_json()),
        )
        .await;
        let job = client_for(&server).await.claim().await.unwrap().unwrap();
        assert_eq!(job.remove_rd_torrent_id, None);
    }

    #[tokio::test]
    async fn the_done_reports_have_the_portals_field_names() {
        let hash = "a".repeat(40);
        let top = "b".repeat(40);
        for (report, expected) in [
            (
                Report::done("ABCDEFGHIJKLM".into(), hash.clone(), Some(top.clone())),
                json!({
                    "claim_id": "c-1", "state": "done", "rd_torrent_id": "ABCDEFGHIJKLM",
                    "chosen_infohash": hash, "top_infohash": top,
                }),
            ),
            (
                Report::already_had(hash.clone()),
                json!({
                    "claim_id": "c-1", "state": "done", "chosen_infohash": hash,
                    "reason": "already_had",
                }),
            ),
            (
                Report::needs_pick_with("picked_failed", "status 403, error_code 35"),
                json!({
                    "claim_id": "c-1", "state": "needs_pick", "reason": "picked_failed",
                    "error": "status 403, error_code 35",
                }),
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/internal/jobs/7/report"))
                .and(body_json(expected))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
                .expect(1)
                .mount(&server)
                .await;
            let result = client_for(&server).await.report(7, "c-1", &report).await;
            assert_eq!(result, Ok(ReportResult::Applied));
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn progress_sends_only_what_it_has_with_the_bearer_token() {
        let cases = [
            (Progress::default(), json!({"claim_id": "c-1"})),
            (
                Progress {
                    rd_torrent_id: Some("ABCDEFGHIJKLM".into()),
                    chosen_infohash: Some("a".repeat(40)),
                    ..Progress::default()
                },
                json!({
                    "claim_id": "c-1", "rd_torrent_id": "ABCDEFGHIJKLM",
                    "chosen_infohash": "a".repeat(40),
                }),
            ),
            (
                Progress {
                    removed_rd_torrent_id: Some("NOPQRSTUVWXYZ".into()),
                    uncached_adds: 1,
                    ..Progress::default()
                },
                json!({
                    "claim_id": "c-1", "removed_rd_torrent_id": "NOPQRSTUVWXYZ",
                    "uncached_adds": 1,
                }),
            ),
        ];
        for (progress, expected) in cases {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/internal/jobs/7/progress"))
                .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
                .and(body_json(expected))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
                .expect(1)
                .mount(&server)
                .await;
            let result = client_for(&server)
                .await
                .progress(7, "c-1", &progress)
                .await;
            assert_eq!(result, Ok(ProgressResult::Applied));
            server.verify().await;
        }
    }

    async fn progress_status(template: ResponseTemplate) -> Result<ProgressResult, PortalError> {
        let server = MockServer::start().await;
        mount(&server, "POST", "/internal/jobs/7/progress", template).await;
        client_for(&server)
            .await
            .progress(7, "c-1", &Progress::default())
            .await
    }

    #[tokio::test]
    async fn progress_maps_the_portals_answers_like_a_report() {
        let stale = ResponseTemplate::new(409)
            .set_body_json(json!({"error": "stale_claim", "rd_torrent_id": "ABC123"}));
        assert_eq!(
            progress_status(stale).await,
            Ok(ProgressResult::Abandon(Abandon::StaleClaim {
                rd_torrent_id: Some("ABC123".into())
            }))
        );
        let stale_bare = ResponseTemplate::new(409).set_body_json(json!({"error": "stale_claim"}));
        assert_eq!(
            progress_status(stale_bare).await,
            Ok(ProgressResult::Abandon(Abandon::StaleClaim {
                rd_torrent_id: None
            }))
        );
        assert_eq!(
            progress_status(ResponseTemplate::new(404)).await,
            Ok(ProgressResult::Abandon(Abandon::NotFound))
        );
        let locked = ResponseTemplate::new(400).set_body_json(json!({"error": "picked_locked"}));
        assert_eq!(
            progress_status(locked).await,
            Ok(ProgressResult::Abandon(Abandon::PickedLocked))
        );
        let bad = ResponseTemplate::new(400).set_body_json(json!({"error": "bad_request"}));
        assert!(matches!(
            progress_status(bad).await,
            Err(PortalError::Fatal(_))
        ));
        assert!(matches!(
            progress_status(ResponseTemplate::new(503)).await,
            Err(PortalError::Transient(_))
        ));
        assert!(matches!(
            progress_status(ResponseTemplate::new(401)).await,
            Err(PortalError::Fatal(text)) if text.starts_with("status 401")
        ));
    }

    #[tokio::test]
    async fn a_failed_report_leaves_out_what_it_does_not_have() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/jobs/7/report"))
            .and(body_json(json!({
                "claim_id": "c-1",
                "state": "failed",
                "reason": "source_unreachable",
                "error": "status 503",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let report = Report::failed(Some("source_unreachable"), "status 503");
        client_for(&server)
            .await
            .report(7, "c-1", &report)
            .await
            .unwrap();
        server.verify().await;

        let bare = serde_json::to_value(Report::failed(None, "boom")).unwrap();
        assert!(bare.get("reason").is_none() && bare.get("candidates").is_none());
    }

    #[test]
    fn an_error_is_cut_to_400_characters() {
        let long = "é".repeat(900);
        let report = Report::failed(None, &long);
        assert_eq!(report.error.as_deref().unwrap().chars().count(), 400);
        assert_eq!(
            Report::failed(None, "short").error.as_deref(),
            Some("short")
        );
    }

    #[tokio::test]
    async fn a_report_that_is_applied_says_so() {
        let result =
            report_status(ResponseTemplate::new(200).set_body_json(json!({"ok": true}))).await;
        assert_eq!(result, Ok(ReportResult::Applied));
    }

    #[tokio::test]
    async fn a_conflict_abandons_with_or_without_the_stored_torrent() {
        let with = report_status(ResponseTemplate::new(409).set_body_json(
            json!({"error": "stale_claim", "message": "x", "rd_torrent_id": "RD1"}),
        ))
        .await;
        assert_eq!(
            with,
            Ok(ReportResult::Abandon(Abandon::StaleClaim {
                rd_torrent_id: Some("RD1".into())
            }))
        );
        let without = report_status(
            ResponseTemplate::new(409)
                .set_body_json(json!({"error": "stale_claim", "message": "x"})),
        )
        .await;
        assert_eq!(
            without,
            Ok(ReportResult::Abandon(Abandon::StaleClaim {
                rd_torrent_id: None
            }))
        );
        // Even a body that is not the portal's still means "not ours any more".
        let odd = report_status(ResponseTemplate::new(409).set_body_string("<html>")).await;
        assert_eq!(
            odd,
            Ok(ReportResult::Abandon(Abandon::StaleClaim {
                rd_torrent_id: None
            }))
        );
    }

    #[tokio::test]
    async fn a_missing_job_and_a_locked_pick_abandon() {
        let gone = report_status(
            ResponseTemplate::new(404).set_body_json(json!({"error": "not_found", "message": "x"})),
        )
        .await;
        assert_eq!(gone, Ok(ReportResult::Abandon(Abandon::NotFound)));
        let locked = report_status(
            ResponseTemplate::new(400)
                .set_body_json(json!({"error": "picked_locked", "message": "x"})),
        )
        .await;
        assert_eq!(locked, Ok(ReportResult::Abandon(Abandon::PickedLocked)));
    }

    #[tokio::test]
    async fn other_bad_requests_and_client_errors_are_fatal() {
        for (code, body) in [
            (400, json!({"error": "bad_request", "message": "x"})),
            (401, json!({"error": "unauthorized", "message": "x"})),
            (403, json!({"error": "forbidden", "message": "x"})),
            (413, json!({})),
            (422, json!({})),
        ] {
            let result = report_status(ResponseTemplate::new(code).set_body_json(body)).await;
            assert!(matches!(result, Err(PortalError::Fatal(_))), "{code}");
        }
        let text = match report_status(
            ResponseTemplate::new(400).set_body_json(json!({"error": "bad_request"})),
        )
        .await
        {
            Err(PortalError::Fatal(text)) => text,
            other => panic!("{other:?}"),
        };
        assert_eq!(text, "status 400 (bad_request)");
    }

    #[tokio::test]
    async fn server_errors_and_timeouts_may_be_retried() {
        for code in [500, 502, 503, 408] {
            let result = report_status(ResponseTemplate::new(code)).await;
            assert!(matches!(result, Err(PortalError::Transient(_))), "{code}");
        }
    }

    #[tokio::test]
    async fn the_same_mapping_holds_for_claim_and_settings() {
        for (code, transient) in [
            (401, false),
            (404, false),
            (409, false),
            (500, true),
            (408, true),
        ] {
            let server = MockServer::start().await;
            mount(
                &server,
                "POST",
                "/internal/jobs/claim",
                ResponseTemplate::new(code),
            )
            .await;
            mount(
                &server,
                "GET",
                "/internal/settings",
                ResponseTemplate::new(code),
            )
            .await;
            let client = client_for(&server).await;
            for error in [
                client.claim().await.unwrap_err(),
                client.settings().await.unwrap_err(),
            ] {
                assert_eq!(
                    matches!(error, PortalError::Transient(_)),
                    transient,
                    "{code}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_refused_connection_may_be_retried_and_names_no_address() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let client = PortalClient::new(&format!("http://127.0.0.1:{port}"), TOKEN).unwrap();
        let error = client.claim().await.unwrap_err();
        assert!(matches!(error, PortalError::Transient(_)));
        assert!(!error.to_string().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn a_redirect_is_not_followed_and_is_fatal() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/x", elsewhere.uri()).as_str()),
        )
        .await;
        assert!(matches!(
            client_for(&server).await.claim().await,
            Err(PortalError::Fatal(_))
        ));
        assert!(elsewhere.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unreadable_answer_is_fatal_and_leaks_nothing() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(200).set_body_string("secret-looking <html> body"),
        )
        .await;
        let error = client_for(&server).await.claim().await.unwrap_err();
        assert!(matches!(error, PortalError::Fatal(_)));
        assert!(!error.to_string().contains("secret"));
    }

    #[tokio::test]
    async fn a_hostile_error_code_is_not_passed_on() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(400).set_body_json(json!({"error": "bad\nTOKEN=x"})),
        )
        .await;
        let error = client_for(&server).await.claim().await.unwrap_err();
        assert_eq!(error, PortalError::Fatal("status 400".into()));
    }

    #[tokio::test]
    async fn the_token_is_never_in_what_the_client_prints() {
        let server = MockServer::start().await;
        let client = client_for(&server).await;
        let shown = format!("{client:?}");
        assert!(!shown.contains(TOKEN) && shown.contains("<redacted>"));
    }

    #[tokio::test]
    async fn the_claim_call_sends_no_body() {
        let server = MockServer::start().await;
        mount(
            &server,
            "POST",
            "/internal/jobs/claim",
            ResponseTemplate::new(204),
        )
        .await;
        client_for(&server).await.claim().await.unwrap();
        let requests: Vec<Request> = server.received_requests().await.unwrap();
        assert!(requests[0].body.is_empty());
    }
}
