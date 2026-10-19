//! The worker-facing `/internal` routes: claim a job, report progress,
//! report the result, read the kill switch. They are served on their own
//! listener (see `internal_router`), never on the public port, and every
//! handler takes `RequireWorker` first, so a missing or wrong token is a
//! 401 before any body is parsed.
//!
//! Everything the worker sends is untrusted text that an admin will later
//! read: it is validated here, and logs carry the job id and state only,
//! never candidates or the job body.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{admin, public::clean_text};
use crate::{
    auth::RequireWorker,
    db::{self, ProgressOutcome, ReportOutcome, ReportState},
    error::{AppError, AppJson, AppPath},
    events::Tick,
    state::AppState,
};

const MAX_CLAIM_ID_CHARS: usize = 128;
const MAX_RD_TORRENT_ID_CHARS: usize = 64;
const MAX_ERROR_CHARS: usize = 500;
const MAX_UNCACHED_ADDS: i64 = 5;
const MAX_CANDIDATES: usize = 20;
/// The serialized shortlist, after cleaning.
const MAX_CANDIDATES_BYTES: usize = 64 * 1024;
const MAX_CANDIDATE_KEYS: usize = 24;
const MAX_CANDIDATE_KEY_CHARS: usize = 64;
const MAX_CANDIDATE_STRING_CHARS: usize = 500;
const MAX_CANDIDATE_LIST_ITEMS: usize = 32;
/// Candidate numbers stay far below where a double loses integers.
const MAX_CANDIDATE_NUMBER: f64 = 1e12;

/// Why a worker ended a run in `needs_pick`, `failed` or `done`.
const REASONS: &[&str] = &[
    "no_imdb_id",
    "no_safe_candidate",
    "dry_run",
    "already_had",
    "picked_failed",
    "worker_disabled",
    "rd_error",
    "timeout",
    "rate_limited",
    "no_candidates",
];

fn ok() -> Json<Value> {
    Json(json!({ "ok": true }))
}

// ---------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------

/// Exactly 40 hex characters, lowercased.
fn clean_infohash(hash: &str) -> Result<String, AppError> {
    if hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hash.to_ascii_lowercase())
    } else {
        Err(AppError::BadRequest)
    }
}

fn clean_optional_infohash(hash: Option<&str>) -> Result<Option<String>, AppError> {
    hash.map(clean_infohash).transpose()
}

/// 1 to 64 ASCII letters and digits.
fn clean_rd_torrent_id(id: Option<&str>) -> Result<Option<String>, AppError> {
    match id {
        None => Ok(None),
        Some(id)
            if (1..=MAX_RD_TORRENT_ID_CHARS).contains(&id.len())
                && id.bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            Ok(Some(id.to_string()))
        }
        Some(_) => Err(AppError::BadRequest),
    }
}

fn clean_claim_id(claim_id: &str) -> Result<&str, AppError> {
    if claim_id.is_empty() || claim_id.chars().count() > MAX_CLAIM_ID_CHARS {
        return Err(AppError::BadRequest);
    }
    Ok(claim_id)
}

fn clean_reason(reason: Option<&str>) -> Result<Option<&str>, AppError> {
    match reason {
        None => Ok(None),
        Some(r) => REASONS
            .iter()
            .find(|known| **known == r)
            .map(|known| Some(*known))
            .ok_or(AppError::BadRequest),
    }
}

fn clean_candidate_string(text: &str) -> Result<String, AppError> {
    let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
    if cleaned.chars().count() > MAX_CANDIDATE_STRING_CHARS {
        return Err(AppError::BadRequest);
    }
    Ok(cleaned)
}

/// A string, a bounded finite number, a bool or null.
fn clean_candidate_scalar(value: Value) -> Result<Value, AppError> {
    match value {
        Value::String(s) => Ok(Value::String(clean_candidate_string(&s)?)),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.is_finite() && f.abs() <= MAX_CANDIDATE_NUMBER => Ok(Value::Number(n)),
            _ => Err(AppError::BadRequest),
        },
        Value::Bool(_) | Value::Null => Ok(value),
        Value::Array(_) | Value::Object(_) => Err(AppError::BadRequest),
    }
}

/// One shortlist entry: a flat object of bounded scalars (a value may also
/// be a short list of them, for tags). Strings lose control characters; an
/// `infohash` key must be a 40 character hex string and is lowercased.
fn clean_candidate(candidate: serde_json::Map<String, Value>) -> Result<Value, AppError> {
    if candidate.len() > MAX_CANDIDATE_KEYS {
        return Err(AppError::BadRequest);
    }
    let mut cleaned = serde_json::Map::new();
    for (key, value) in candidate {
        if key.is_empty() || key.chars().count() > MAX_CANDIDATE_KEY_CHARS {
            return Err(AppError::BadRequest);
        }
        let value = match value {
            Value::Array(items) => {
                if items.len() > MAX_CANDIDATE_LIST_ITEMS {
                    return Err(AppError::BadRequest);
                }
                Value::Array(
                    items
                        .into_iter()
                        .map(clean_candidate_scalar)
                        .collect::<Result<_, _>>()?,
                )
            }
            other => clean_candidate_scalar(other)?,
        };
        let value = if key == "infohash" {
            match &value {
                Value::String(hash) => Value::String(clean_infohash(hash)?),
                _ => return Err(AppError::BadRequest),
            }
        } else {
            value
        };
        cleaned.insert(key, value);
    }
    Ok(Value::Object(cleaned))
}

/// The shortlist as the JSON text to store, or an error when it is too
/// long, too big or malformed.
fn clean_candidates(
    candidates: Option<Vec<serde_json::Map<String, Value>>>,
) -> Result<Option<String>, AppError> {
    let Some(candidates) = candidates else {
        return Ok(None);
    };
    if candidates.len() > MAX_CANDIDATES {
        return Err(AppError::BadRequest);
    }
    let cleaned: Vec<Value> = candidates
        .into_iter()
        .map(clean_candidate)
        .collect::<Result<_, _>>()?;
    let text = serde_json::to_string(&cleaned).map_err(|_| AppError::BadRequest)?;
    if text.len() > MAX_CANDIDATES_BYTES {
        return Err(AppError::BadRequest);
    }
    Ok(Some(text))
}

// ---------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------

/// What the worker is told about a job. Spelled out field by field, so a
/// column added to `ClaimedJob` later does not reach the worker by accident.
#[derive(Serialize)]
struct ClaimView {
    id: i64,
    claim_id: String,
    tmdb_id: i64,
    imdb_id: String,
    media_type: String,
    title: String,
    year: Option<i64>,
    infuse_pro: bool,
    chosen_infohash: Option<String>,
    /// A human chose `chosen_infohash`: use exactly that hash, never fall
    /// back to another candidate.
    picked: bool,
    rd_torrent_id: Option<String>,
    blocked_hashes: Vec<String>,
}

/// `POST /internal/jobs/claim`: 200 with the next job, or 204 when there is
/// nothing to do (or the kill switch is off).
pub async fn post_claim(
    _worker: RequireWorker,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let settings = db::worker_settings(&state.pool, &state.config).await?;
    let claim =
        db::claim_fetch_job_with_changes(&state.pool, settings.enabled, admin::now(&state)).await?;
    // A tick for what the admin can see change: jobs the tidy-up moved, and a
    // job handed out. An idle poll changes nothing and ticks nothing.
    if claim.tidied || claim.job.is_some() {
        state.events.publish(Tick::Admin);
    }
    let Some(job) = claim.job else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    tracing::info!(job_id = job.id, state = "fetching", "fetch job claimed");
    Ok(Json(ClaimView {
        id: job.id,
        claim_id: job.claim_id,
        tmdb_id: job.tmdb_id,
        imdb_id: job.imdb_id,
        media_type: job.media_type,
        title: job.title,
        year: job.year,
        infuse_pro: job.infuse_pro,
        chosen_infohash: job.chosen_infohash,
        picked: job.picked,
        rd_torrent_id: job.rd_torrent_id,
        blocked_hashes: job.blocked_hashes,
    })
    .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressBody {
    claim_id: String,
    rd_torrent_id: Option<String>,
    chosen_infohash: Option<String>,
    uncached_adds: Option<i64>,
}

/// `POST /internal/jobs/{id}/progress`: a sign of life that also saves a
/// torrent id or hash. No tick: nothing here is user-visible.
pub async fn post_progress(
    _worker: RequireWorker,
    State(state): State<AppState>,
    AppPath(id): AppPath<i64>,
    AppJson(body): AppJson<ProgressBody>,
) -> Result<Json<Value>, AppError> {
    let claim_id = clean_claim_id(&body.claim_id)?;
    let rd_torrent_id = clean_rd_torrent_id(body.rd_torrent_id.as_deref())?;
    let chosen_infohash = clean_optional_infohash(body.chosen_infohash.as_deref())?;
    let uncached_adds = body.uncached_adds.unwrap_or(0);
    if !(0..=MAX_UNCACHED_ADDS).contains(&uncached_adds) {
        return Err(AppError::BadRequest);
    }
    let progress = db::Progress {
        rd_torrent_id: rd_torrent_id.as_deref(),
        chosen_infohash: chosen_infohash.as_deref(),
        uncached_adds,
    };
    match db::progress_fetch_job(&state.pool, id, claim_id, &progress, admin::now(&state)).await? {
        ProgressOutcome::Applied => {
            tracing::debug!(job_id = id, "fetch job progress");
            Ok(ok())
        }
        ProgressOutcome::Stale { rd_torrent_id } => Err(AppError::StaleClaim { rd_torrent_id }),
        ProgressOutcome::PickedLocked => Err(AppError::PickedLocked),
        ProgressOutcome::NotFound => Err(AppError::NotFound),
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReportStateBody {
    NeedsPick,
    Failed,
    Done,
}

impl From<ReportStateBody> for ReportState {
    fn from(state: ReportStateBody) -> Self {
        match state {
            ReportStateBody::NeedsPick => ReportState::NeedsPick,
            ReportStateBody::Failed => ReportState::Failed,
            ReportStateBody::Done => ReportState::Done,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportBody {
    claim_id: String,
    state: ReportStateBody,
    candidates: Option<Vec<serde_json::Map<String, Value>>>,
    top_infohash: Option<String>,
    chosen_infohash: Option<String>,
    rd_torrent_id: Option<String>,
    reason: Option<String>,
    error: Option<String>,
}

/// `POST /internal/jobs/{id}/report`: ends a run as `needs_pick`, `failed`
/// or `done`.
pub async fn post_report(
    _worker: RequireWorker,
    State(state): State<AppState>,
    AppPath(id): AppPath<i64>,
    AppJson(body): AppJson<ReportBody>,
) -> Result<Json<Value>, AppError> {
    let claim_id = clean_claim_id(&body.claim_id)?;
    let candidates = clean_candidates(body.candidates)?;
    let top_infohash = clean_optional_infohash(body.top_infohash.as_deref())?;
    let chosen_infohash = clean_optional_infohash(body.chosen_infohash.as_deref())?;
    let rd_torrent_id = clean_rd_torrent_id(body.rd_torrent_id.as_deref())?;
    let reason = clean_reason(body.reason.as_deref())?;
    let last_error = clean_text(body.error.as_deref(), MAX_ERROR_CHARS)?;
    let report_state = ReportState::from(body.state);

    let report = db::Report {
        candidates: candidates.as_deref(),
        top_infohash: top_infohash.as_deref(),
        chosen_infohash: chosen_infohash.as_deref(),
        rd_torrent_id: rd_torrent_id.as_deref(),
        reason,
        last_error: last_error.as_deref(),
        uncached_adds: 0,
    };
    match db::report_fetch_job(
        &state.pool,
        id,
        claim_id,
        report_state,
        &report,
        admin::now(&state),
    )
    .await?
    {
        ReportOutcome::Applied {
            request_status_changed,
        } => {
            tracing::info!(
                job_id = id,
                state = report_state.as_str(),
                "fetch job reported"
            );
            state.events.publish(Tick::Admin);
            if request_status_changed {
                state.events.publish(Tick::Members);
            }
            Ok(ok())
        }
        ReportOutcome::Stale { rd_torrent_id } => Err(AppError::StaleClaim { rd_torrent_id }),
        ReportOutcome::PickedLocked => Err(AppError::PickedLocked),
        ReportOutcome::MissingTorrent => Err(AppError::BadRequest),
        ReportOutcome::NotFound => Err(AppError::NotFound),
    }
}

/// `GET /internal/settings`: the kill switch and mode the worker obeys.
pub async fn get_settings(
    _worker: RequireWorker,
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let settings = db::worker_settings(&state.pool, &state.config).await?;
    Ok(Json(json!({
        "enabled": settings.enabled,
        "mode": settings.mode.as_str(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(value: Value) -> serde_json::Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn infohashes_are_forty_hex_characters_and_lowercased() {
        let upper = "ABCDEF0123456789ABCDEF0123456789ABCDEF01";
        assert_eq!(clean_infohash(upper).unwrap(), upper.to_ascii_lowercase());
        assert!(clean_infohash(&upper[..39]).is_err());
        assert!(clean_infohash(&format!("{upper}0")).is_err());
        assert!(clean_infohash(&upper.replace('A', "g")).is_err());
        assert!(clean_infohash("").is_err());
    }

    #[test]
    fn torrent_ids_are_short_and_alphanumeric() {
        assert_eq!(clean_rd_torrent_id(None).unwrap(), None);
        assert!(clean_rd_torrent_id(Some("ABC123xyz")).unwrap().is_some());
        assert!(
            clean_rd_torrent_id(Some(&"A".repeat(64)))
                .unwrap()
                .is_some()
        );
        assert!(clean_rd_torrent_id(Some(&"A".repeat(65))).is_err());
        assert!(clean_rd_torrent_id(Some("")).is_err());
        assert!(clean_rd_torrent_id(Some("a-b")).is_err());
        assert!(clean_rd_torrent_id(Some("a b")).is_err());
    }

    #[test]
    fn reasons_come_from_the_fixed_list() {
        assert_eq!(clean_reason(Some("dry_run")).unwrap(), Some("dry_run"));
        assert!(clean_reason(Some("cancelled")).is_err());
        assert!(clean_reason(Some("anything")).is_err());
    }

    #[test]
    fn candidates_are_cleaned_and_bounded() {
        let cleaned = clean_candidates(Some(vec![object(json!({
            "title": "Dune\n2021\u{7}",
            "infohash": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "score": 12.5,
            "tags": ["aac", "x265"],
        }))]))
        .unwrap()
        .unwrap();
        let parsed: Value = serde_json::from_str(&cleaned).unwrap();
        assert_eq!(parsed[0]["title"], "Dune2021");
        assert_eq!(
            parsed[0]["infohash"],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        let many = vec![object(json!({"a": 1})); MAX_CANDIDATES + 1];
        assert!(clean_candidates(Some(many)).is_err());
        assert!(clean_candidates(Some(vec![object(json!({"nested": {"a": 1}}))])).is_err());
        assert!(clean_candidates(Some(vec![object(json!({"n": 1e15}))])).is_err());
        assert!(clean_candidates(Some(vec![object(json!({"infohash": "short"}))])).is_err());
        let long = "x".repeat(MAX_CANDIDATE_STRING_CHARS + 1);
        assert!(clean_candidates(Some(vec![object(json!({ "t": long }))])).is_err());
        // Twenty entries that are each fine, but together past 64 KiB.
        let fat: serde_json::Map<String, Value> = (0..10)
            .map(|i| {
                (
                    format!("k{i}"),
                    json!("x".repeat(MAX_CANDIDATE_STRING_CHARS)),
                )
            })
            .collect();
        assert!(clean_candidates(Some(vec![fat; MAX_CANDIDATES])).is_err());
        assert_eq!(clean_candidates(None).unwrap(), None);
    }
}
