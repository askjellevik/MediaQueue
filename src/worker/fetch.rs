//! The Real-Debrid run for a job. A human's pick is the one hash the admin
//! chose, put into the library, checked and reported; it never falls back to
//! another release. An auto job walks the candidates the ranking planned
//! (see [`auto`]), each one through the same steps, and moves to the next
//! only when one is refused, unusable or too slow.
//!
//! The order is fixed. A removal request comes first. Then, for a hash: the
//! gates (breaker, kill switch, daily caps, free slots), a look in the
//! library, the add (counted in the state file before it happens, owned the
//! moment Real-Debrid names the new torrent), the file choice, the selection
//! of exactly one file, a wait, and a check of what finished. Whatever goes
//! wrong, a torrent this run created is deleted again; a torrent it does not
//! own is never touched.
//!
//! Everything here ends in a [`Report`] for the portal (or in nothing, when
//! the claim is gone). Log lines carry job ids and hashes only.

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};

use serde_json::{Map, Value};

use super::{
    files::{self, Choice, FileRules},
    portal::{Abandon, Job, Mode, PortalClient, Progress, ProgressResult, Report},
    rd::{ListedTorrent, RdError, RdRuntime, TorrentInfo, TorrentStatus, current_job, valid_id},
    rules::FreeBadge,
    run::Timing,
    score::Evaluation,
    state::{AddKind, StateError},
};

/// Failed reads of the worker settings in a row that a wait puts up with.
/// The checks before an add and before a selection allow none.
const SETTINGS_MISSES: u32 = 3;

/// What a run needs besides the job.
pub struct Env<'a> {
    pub rd: &'a RdRuntime,
    pub portal: &'a PortalClient,
    pub timing: &'a Timing,
    /// The largest file an auto job may take, on top of the Real-Debrid cap.
    pub auto_max_bytes: u64,
}

impl Env<'_> {
    fn picked_terms(&self) -> Terms {
        Terms {
            kind: AddKind::Picked,
            wait: self.timing.rd_picked_wait,
            cap: self.rd.max_bytes,
        }
    }

    fn auto_terms(&self) -> Terms {
        Terms {
            kind: AddKind::Auto,
            wait: self.timing.rd_auto_wait,
            cap: self.rd.max_bytes.min(self.auto_max_bytes),
        }
    }
}

/// What differs between a torrent a human picked and one the worker chose:
/// which counters it touches, how long a download may take after its file
/// was selected, and the biggest file it may have.
#[derive(Debug, Clone, Copy)]
struct Terms {
    kind: AddKind,
    wait: Duration,
    cap: u64,
}

/// The torrent this run created or resumed, kept outside the run so it can
/// still be deleted when the run is cut short by its deadline. An auto run
/// also parks the shortlist here, so a report that ends it can carry it.
#[derive(Debug, Default)]
pub struct Trace {
    pub torrent: Option<String>,
    pub candidates: Vec<Map<String, Value>>,
    pub top_infohash: Option<String>,
}

pub enum Outcome {
    Report(Report),
    /// The claim is gone (retried, cancelled, picked again, reclaimed):
    /// there is nothing to report.
    Dropped,
}

/// Why one hash did not work out. A picked run reports every one of them as
/// `picked_failed`; an auto run decides from the kind whether to block the
/// hash and which reason to give if it was the last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// Real-Debrid refused it or called it bad (error 35, `virus`, `dead`,
    /// `magnet_error`, `error`), or its files are unsafe. Blocked for good.
    Refused,
    /// Not usable, nothing against the hash: ambiguous files, no video,
    /// over the size cap, a failed check.
    Unfit,
    /// Not downloaded in time.
    Timeout,
    /// A failed call, a torrent that vanished, an answer nobody can read.
    Error,
    /// Already in the library but not downloaded; left alone.
    Library,
}

impl Fail {
    /// The reason an auto run ends with when this was the last failure.
    fn reason(self) -> &'static str {
        match self {
            Fail::Refused | Fail::Unfit => "rd_refused",
            Fail::Timeout => "rd_timeout",
            Fail::Error | Fail::Library => "rd_error",
        }
    }
}

enum Stop {
    /// The run is over with this report (a gate, or a pause).
    Report(Box<Report>),
    /// One hash failed; an auto run may go on with the next.
    Failed(Fail, String),
    /// The portal said the claim is no longer ours. `keep` is the torrent
    /// the portal now holds for the job, which must not be deleted.
    Dropped { keep: Option<String> },
}

type Step<T> = Result<T, Stop>;

fn failed(fail: Fail, text: impl AsRef<str>) -> Stop {
    Stop::Failed(fail, text.as_ref().to_string())
}

/// The report for a hash a human picked that failed: always `picked_failed`,
/// except a hit in the library that is not downloaded, which is `rd_error`.
fn picked_report(fail: Fail, text: &str) -> Report {
    match fail {
        Fail::Library => Report::needs_pick_with("rd_error", text),
        _ => Report::needs_pick_with("picked_failed", text),
    }
}

fn gate(reason: &'static str, text: &str) -> Stop {
    Stop::Report(Box::new(Report::needs_pick_with(reason, text)))
}

/// An error from Real-Debrid: the breaker is a pause that ends the run,
/// anything else is a failure of the hash at hand.
fn rd_stop(error: RdError) -> Stop {
    match error {
        RdError::Breaker(_) => gate("rate_limited", "real-debrid is being left alone"),
        RdError::Refused(code) => failed(
            Fail::Refused,
            format!("real-debrid refused the torrent (error {code})"),
        ),
        RdError::NotFound => failed(Fail::Error, "the torrent is gone from real-debrid"),
        RdError::Transient => failed(Fail::Error, "real-debrid is temporarily unavailable"),
        RdError::Fatal => failed(Fail::Error, "real-debrid sent an unexpected answer"),
    }
}

/// One line when a run ends in a timeout, a refusal or the deletion of an
/// owned torrent: the job and a fixed reason code, nothing else.
pub fn run_ended(job: i64, reason: &'static str) {
    tracing::info!(job, reason, "run ended");
}

/// Deletes a torrent this worker owns. A torrent that is already gone is
/// fine; one that cannot be deleted now is queued for later. Returns false
/// only when even that could not be saved.
async fn delete_owned(rd: &RdRuntime, id: &str) -> bool {
    if !rd.state.owns(id) {
        tracing::info!("not deleting a torrent this worker did not create");
        return true;
    }
    match rd.client.delete(id).await {
        Ok(()) | Err(RdError::NotFound) => {
            run_ended(current_job(), "owned_torrent_deleted");
            if let Err(error) = rd.state.forget(id) {
                tracing::warn!(%error, "could not update the owned list");
            }
            true
        }
        Err(error) => {
            tracing::warn!(%error, "torrent not deleted now; queued for later");
            match rd.state.pending_delete_add(id) {
                Ok(()) => true,
                Err(error) => {
                    tracing::error!(%error, "could not queue the deletion");
                    false
                }
            }
        }
    }
}

/// Deletes what the run created, unless the portal named it as the job's
/// own torrent.
pub async fn cleanup(env: &Env<'_>, trace: &mut Trace, keep: Option<&str>) {
    let Some(id) = trace.torrent.take() else {
        return;
    };
    if keep == Some(id.as_str()) {
        return;
    }
    delete_owned(env.rd, &id).await;
}

/// A removal the admin asked for (a cancelled job, a release that did not
/// play): deleted only if this worker created the torrent *for a hash the
/// claim itself ties to the request*, the one chosen or one blocked. Then
/// cleared at the portal. Never trusts the portal about ownership; a torrent
/// the worker owns for another title is left alone.
pub async fn handle_removal(env: &Env<'_>, job: &Job) {
    let Some(id) = job.remove_rd_torrent_id.as_deref() else {
        return;
    };
    if !valid_id(id) {
        tracing::warn!(job = job.id, "ignoring a malformed removal id");
        return;
    }
    if removable(env, job, id) {
        if !delete_owned(env.rd, id).await {
            // Not recorded anywhere here, so leave it at the portal.
            return;
        }
    } else {
        tracing::info!(
            job = job.id,
            "removal request for a torrent not owned here; cleared only"
        );
    }
    let progress = Progress {
        removed_rd_torrent_id: Some(id.to_string()),
        ..Progress::default()
    };
    if let Err(error) = env.portal.progress(job.id, &job.claim_id, &progress).await {
        tracing::warn!(job = job.id, %error, "could not clear the removal at the portal");
    }
}

/// True when `id` is owned here and its hash is the job's chosen one or one
/// of its blocked ones.
fn removable(env: &Env<'_>, job: &Job, id: &str) -> bool {
    let Some(owned) = env.rd.state.owned_hash(id) else {
        return false;
    };
    job.chosen_infohash
        .iter()
        .chain(&job.blocked_hashes)
        .any(|hash| hash.eq_ignore_ascii_case(&owned))
}

/// Retries deletions that could not be made earlier (the breaker was open,
/// Real-Debrid was down). Does nothing while the breaker is open. An id
/// leaves the persisted list only after Real-Debrid has answered the delete
/// (or says the torrent is gone), so a crash in between loses nothing.
pub async fn sweep_pending(rd: &RdRuntime) {
    if rd.state.breaker_trip(rd.state.now()).is_some() {
        return;
    }
    for id in rd.state.pending_delete_ids() {
        if !rd.state.owns(&id) {
            // Not ours to delete; nothing to retry.
            let _ = rd.state.pending_delete_done(&id);
            continue;
        }
        match rd.client.delete(&id).await {
            Ok(()) | Err(RdError::NotFound) => {
                tracing::info!("a queued deletion went through");
                if let Err(error) = rd.state.pending_delete_done(&id) {
                    tracing::warn!(%error, "could not update the queued deletions");
                }
                let _ = rd.state.forget(&id);
            }
            Err(error) => tracing::warn!(%error, "a queued deletion failed again"),
        }
    }
}

/// The whole run of a picked job. `trace` names the torrent to delete if
/// the caller has to cut the run short.
pub async fn picked(env: &Env<'_>, job: &Job, trace: &mut Trace) -> Outcome {
    match drive(env, job, trace).await {
        Ok(report) => {
            // Done: the torrent is the result and stays.
            trace.torrent = None;
            Outcome::Report(report)
        }
        Err(Stop::Report(report)) => {
            cleanup(env, trace, None).await;
            Outcome::Report(*report)
        }
        Err(Stop::Failed(fail, text)) => {
            cleanup(env, trace, None).await;
            Outcome::Report(picked_report(fail, &text))
        }
        Err(Stop::Dropped { keep }) => {
            cleanup(env, trace, keep.as_deref()).await;
            Outcome::Dropped
        }
    }
}

/// The hashes an auto job may take, best first: the ranking's planned
/// list, checked again here so no change over there can widen it. A hash is
/// in only if it is 40 hex characters, has an auto rank and is in the
/// ranking's own list, its size is known and within `cap`, and, for a free
/// job, its badge is `yes`.
pub fn eligible_hashes(evaluation: &Evaluation, pro: bool, cap: u64) -> Vec<String> {
    let mut picks: Vec<_> = evaluation
        .shortlist
        .iter()
        .filter(|c| {
            let hash = c.raw.infohash.as_str();
            c.auto_rank > 0
                && evaluation.auto.iter().any(|planned| planned == hash)
                && hash.len() == 40
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && c.raw.size_bytes.is_some_and(|n| n <= cap)
                && (pro || c.badge == FreeBadge::Yes)
        })
        .collect();
    picks.sort_by_key(|c| c.auto_rank);
    picks
        .into_iter()
        .map(|c| c.raw.infohash.to_ascii_lowercase())
        .collect()
}

/// The run of an auto job: the planned candidates in order, each one a full
/// pass (library, add, file choice, selection, wait, check). A candidate
/// that fails is deleted again and the next planned one is tried, never a
/// near-duplicate of it (the plan already folded those away). Real-Debrid's
/// refusals are blocked at the portal; a timeout is not. When every
/// candidate failed the job goes back to the admin with the last reason.
/// Nothing eligible means no call to Real-Debrid at all.
pub async fn auto(env: &Env<'_>, job: &Job, evaluation: &Evaluation, trace: &mut Trace) -> Outcome {
    let terms = env.auto_terms();
    let picks = eligible_hashes(evaluation, job.infuse_pro, terms.cap);
    if picks.is_empty() {
        return Outcome::Report(Report::needs_pick_with(
            "no_clear_choice",
            "no candidate is safe to take without a person",
        ));
    }
    let mut refused: Vec<String> = Vec::new();
    let result = walk(env, job, trace, terms, &picks, &mut refused).await;
    let mut report = match result {
        Ok(report) => {
            // Done: the torrent is the result and stays.
            trace.torrent = None;
            report
        }
        Err(Stop::Report(report)) => {
            cleanup(env, trace, None).await;
            *report
        }
        Err(Stop::Failed(fail, text)) => {
            cleanup(env, trace, None).await;
            Report::needs_pick_with(fail.reason(), &text)
        }
        Err(Stop::Dropped { keep }) => {
            cleanup(env, trace, keep.as_deref()).await;
            return Outcome::Dropped;
        }
    };
    report.blocked = most_recent(&refused);
    Outcome::Report(report)
}

/// The last five (the portal takes no more per call).
fn most_recent(hashes: &[String]) -> Vec<String> {
    hashes[hashes.len().saturating_sub(5)..].to_vec()
}

async fn walk(
    env: &Env<'_>,
    job: &Job,
    trace: &mut Trace,
    terms: Terms,
    picks: &[String],
    refused: &mut Vec<String>,
) -> Step<Report> {
    gate_switch_and_breaker(env).await?;
    let snapshot = env.rd.client.list_torrents().await.map_err(rd_stop)?;
    // Every planned hash is looked for in the library before anything is
    // added, so a release that is already there wins over a new add.
    let in_library = |hash: &&String| snapshot.iter().any(|t| t.hash == **hash);
    let order: Vec<&String> = picks
        .iter()
        .filter(in_library)
        .chain(picks.iter().filter(|hash| !in_library(hash)))
        .collect();

    let mut last: Option<(Fail, String)> = None;
    for (n, hash) in order.iter().enumerate() {
        match candidate(env, job, trace, terms, picks, hash, &snapshot).await {
            Ok(report) => return Ok(report),
            Err(Stop::Failed(fail, text)) => {
                cleanup(env, trace, None).await;
                tracing::info!(job = job.id, %hash, ?fail, "candidate failed");
                if fail == Fail::Refused {
                    refused.push((*hash).clone());
                }
                last = Some((fail, text));
                if let Some(next) = order.get(n + 1) {
                    let progress = Progress {
                        chosen_infohash: Some((*next).clone()),
                        blocked: most_recent(refused),
                        ..Progress::default()
                    };
                    tell_portal(env, job, progress).await?;
                }
            }
            Err(stop) => return Err(stop),
        }
    }
    let (fail, text) = last.unwrap_or((Fail::Error, "no candidate was tried".to_string()));
    run_ended(job.id, fail.reason());
    Ok(Report::needs_pick_with(
        fail.reason(),
        &format!("{} of {} tried; last: {text}", order.len(), picks.len()),
    ))
}

/// One planned hash through the library check and the add. Only a hash on
/// the eligible list is ever touched.
async fn candidate(
    env: &Env<'_>,
    job: &Job,
    trace: &mut Trace,
    terms: Terms,
    picks: &[String],
    hash: &str,
    snapshot: &[ListedTorrent],
) -> Step<Report> {
    if !picks.iter().any(|planned| planned == hash) {
        return Err(failed(Fail::Unfit, "not a planned candidate"));
    }
    if let Some(hit) = snapshot.iter().find(|t| t.hash == hash)
        && let Some(report) = library_hit(env, job, hit, hash, trace, terms).await?
    {
        return Ok(report);
    }
    add_and_finish(env, job, trace, terms, hash, snapshot).await
}

async fn drive(env: &Env<'_>, job: &Job, trace: &mut Trace) -> Step<Report> {
    let hash = match job.chosen_infohash.as_deref() {
        Some(hash) if hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit()) => {
            hash.to_ascii_lowercase()
        }
        _ => return Err(failed(Fail::Unfit, "the picked release has no valid hash")),
    };
    let terms = env.picked_terms();

    gate_switch_and_breaker(env).await?;

    // Resume a torrent an earlier run of this worker added for this very
    // hash; one it owns for another hash is not this job's.
    if let Some(id) = job.rd_torrent_id.as_deref()
        && env
            .rd
            .state
            .owned_hash(id)
            .is_some_and(|owned| owned.eq_ignore_ascii_case(&hash))
    {
        match env.rd.client.info(id).await {
            Ok(_) => {
                tracing::info!(job = job.id, "resuming an owned torrent");
                trace.torrent = Some(id.to_string());
                return finish(env, job, id, &hash, terms).await;
            }
            Err(RdError::NotFound) => {
                tracing::info!(job = job.id, "the owned torrent is gone; adding again");
                let _ = env.rd.state.forget(id);
            }
            Err(error) => return Err(rd_stop(error)),
        }
    }

    let snapshot = env.rd.client.list_torrents().await.map_err(rd_stop)?;
    if let Some(hit) = snapshot.iter().find(|t| t.hash == hash)
        && let Some(report) = library_hit(env, job, hit, &hash, trace, terms).await?
    {
        return Ok(report);
    }
    add_and_finish(env, job, trace, terms, &hash, &snapshot).await
}

/// From a hash that is not in the library to a verified result: the gates,
/// the counted add, the record of ownership, then [`finish`].
async fn add_and_finish(
    env: &Env<'_>,
    job: &Job,
    trace: &mut Trace,
    terms: Terms,
    hash: &str,
    snapshot: &[ListedTorrent],
) -> Step<Report> {
    let kind = terms.kind;
    gate_caps_and_slots(env, kind).await?;
    // The last look before anything is added: the slot check above is a
    // network call, and an admin may have switched the worker off meanwhile.
    require_switch_on(env).await?;
    env.rd.state.record_add(kind).map_err(|error| match error {
        StateError::CapReached => gate("daily_cap", "the daily cap of adds is reached"),
        _ => gate("rd_error", "the counters file could not be written"),
    })?;
    let id = match env.rd.client.add_magnet(hash).await {
        Ok(id) => id,
        Err(error) => {
            // Only a refusal classified by Real-Debrid's own error code gives
            // the add back; a network error, a 5xx or an answer nobody can
            // read leaves it counted, since the torrent may have been taken.
            if matches!(error, RdError::Refused(_)) {
                run_ended(job.id, "rd_refused");
                if let Err(error) = env.rd.state.refund_refused_add(kind) {
                    tracing::warn!(job = job.id, %error, "could not refund the refused add");
                }
            }
            return Err(rd_stop(error));
        }
    };
    let known: HashSet<&str> = snapshot.iter().map(|t| t.id.as_str()).collect();
    if known.contains(id.as_str()) {
        // Real-Debrid answered with a torrent that was already there; it is
        // not ours to take over or to delete. One line, so a supervised run
        // can tell whether addMagnet hands back an existing id.
        tracing::warn!(
            job = job.id,
            "addMagnet returned an id that was already in the library; left alone"
        );
        return Err(failed(
            Fail::Error,
            "real-debrid returned a torrent that already existed",
        ));
    }
    if env.rd.state.own(&id, hash).is_err() {
        // Without a record it could never be cleaned up later.
        let _ = env.rd.client.delete(&id).await;
        return Err(gate("rd_error", "the counters file could not be written"));
    }
    trace.torrent = Some(id.clone());
    tracing::info!(
        job = job.id,
        picked_adds_today = env.rd.state.picked_adds_today(),
        "added to real-debrid"
    );
    tell_portal(
        env,
        job,
        Progress {
            rd_torrent_id: Some(id.clone()),
            chosen_infohash: Some(hash.to_string()),
            ..Progress::default()
        },
    )
    .await?;

    finish(env, job, &id, hash, terms).await
}

/// The breaker and the kill switch, checked before anything is done.
async fn gate_switch_and_breaker(env: &Env<'_>) -> Step<()> {
    if env.rd.state.breaker_trip(env.rd.state.now()).is_some() {
        return Err(gate("rate_limited", "real-debrid is being left alone"));
    }
    require_switch_on(env).await
}

/// Reads the kill switch and stops unless it is plainly on. A read that
/// fails counts as off: nothing is added or selected on a guess.
async fn require_switch_on(env: &Env<'_>) -> Step<()> {
    match env.portal.settings().await {
        Ok(settings) if settings.enabled && settings.mode != Mode::DryRun => Ok(()),
        Ok(_) => Err(switched_off()),
        Err(error) => {
            tracing::warn!(%error, "could not re-read the worker settings");
            Err(gate(
                "rd_error",
                "could not confirm that the worker is switched on",
            ))
        }
    }
}

fn switched_off() -> Stop {
    gate(
        "worker_disabled",
        "the worker was switched off during the run",
    )
}

/// Before an add: the counters must be readable, a job the worker chose
/// itself must be within today's caps (a human's pick is not stopped by
/// them), and a free active slot must be left.
async fn gate_caps_and_slots(env: &Env<'_>, kind: AddKind) -> Step<()> {
    if env.rd.state.unreadable() {
        return Err(gate("rd_error", "the counters file cannot be trusted"));
    }
    if kind == AddKind::Auto && env.rd.state.auto_caps_reached() {
        return Err(gate("daily_cap", "the daily cap of adds is reached"));
    }
    let active = env.rd.client.active_count().await.map_err(rd_stop)?;
    if active.nb.saturating_add(env.rd.keep_free_slots) >= active.limit {
        return Err(gate("rd_slots_full", "no free active slot at real-debrid"));
    }
    Ok(())
}

/// The picked hash is already in the library. `Ok(Some)` ends the run,
/// `Ok(None)` means carry on as if it were not there.
async fn library_hit(
    env: &Env<'_>,
    job: &Job,
    hit: &ListedTorrent,
    hash: &str,
    trace: &mut Trace,
    terms: Terms,
) -> Step<Option<Report>> {
    if env.rd.state.owns(&hit.id) {
        // Ours from an earlier run whose id the portal no longer holds.
        tracing::info!(job = job.id, "found an owned torrent for the hash");
        trace.torrent = Some(hit.id.clone());
        tell_portal(
            env,
            job,
            Progress {
                rd_torrent_id: Some(hit.id.clone()),
                chosen_infohash: Some(hash.to_string()),
                ..Progress::default()
            },
        )
        .await?;
        return finish(env, job, &hit.id, hash, terms).await.map(Some);
    }
    let info = match env.rd.client.info(&hit.id).await {
        Ok(info) => info,
        Err(RdError::NotFound) => return Ok(None),
        Err(error) => return Err(rd_stop(error)),
    };
    if info.status == TorrentStatus::Downloaded
        && files::has_selected_video(&info, &FileRules::default(), terms.cap)
    {
        tracing::info!(job = job.id, "already in the real-debrid library");
        return Ok(Some(Report::already_had(hash.to_string())));
    }
    // Not ours and not usable: leave it alone and let the admin look.
    Err(failed(
        Fail::Library,
        "the release is in the library but is not downloaded",
    ))
}

/// A look at the world while waiting: the kill switch must still be on and
/// the claim still ours. A few failed reads in a row are put up with
/// (`misses` counts them); one more ends the run.
async fn checkpoint(env: &Env<'_>, job: &Job, misses: &mut u32) -> Step<()> {
    match env.portal.settings().await {
        Ok(settings) if settings.enabled && settings.mode != Mode::DryRun => *misses = 0,
        Ok(_) => return Err(switched_off()),
        Err(error) => {
            *misses += 1;
            tracing::warn!(job = job.id, %error, misses = *misses, "could not re-read the worker settings");
            if *misses > SETTINGS_MISSES {
                return Err(gate(
                    "rd_error",
                    "could not confirm that the worker is switched on",
                ));
            }
        }
    }
    tell_portal(env, job, Progress::default()).await
}

/// The look before a file is selected: no failed read is put up with.
async fn checkpoint_strict(env: &Env<'_>, job: &Job) -> Step<()> {
    require_switch_on(env).await?;
    tell_portal(env, job, Progress::default()).await
}

/// Sends a progress call. A refused claim ends the run; a portal that
/// cannot be reached does not, because the final report carries the same ids.
async fn tell_portal(env: &Env<'_>, job: &Job, progress: Progress) -> Step<()> {
    match env.portal.progress(job.id, &job.claim_id, &progress).await {
        Ok(ProgressResult::Applied) => Ok(()),
        Ok(ProgressResult::Abandon(why)) => {
            tracing::info!(job = job.id, "claim no longer valid, run abandoned");
            let keep = match why {
                Abandon::StaleClaim { rd_torrent_id } => rd_torrent_id,
                Abandon::NotFound | Abandon::PickedLocked => None,
            };
            Err(Stop::Dropped { keep })
        }
        Err(error) => {
            tracing::warn!(job = job.id, %error, "progress call failed");
            Ok(())
        }
    }
}

/// From a torrent id to a verified result: wait for the file list, choose
/// and select one file, wait for the download, check it.
async fn finish(env: &Env<'_>, job: &Job, id: &str, hash: &str, terms: Terms) -> Step<Report> {
    let rules = FileRules::default();
    let started = Instant::now();
    let mut chosen: Option<u64> = None;
    let mut selected_at: Option<Instant> = None;
    let mut polls = 0usize;
    let mut misses = 0u32;

    loop {
        let info = env.rd.client.info(id).await.map_err(rd_stop)?;
        if !info.hash.is_empty() && !info.hash.eq_ignore_ascii_case(hash) {
            return Err(failed(
                Fail::Unfit,
                "the torrent's hash differs from the one asked for",
            ));
        }
        if chosen.is_none() && !info.files.is_empty() {
            chosen = Some(settle_files(&info, &rules, terms)?);
            if info.files.iter().any(|f| f.selected) {
                selected_at = Some(Instant::now());
            } else if info.status == TorrentStatus::WaitingFilesSelection
                && let Some(file_id) = chosen
            {
                checkpoint_strict(env, job).await?;
                env.rd
                    .client
                    .select_files(id, file_id)
                    .await
                    .map_err(rd_stop)?;
                selected_at = Some(Instant::now());
                tracing::info!(job = job.id, "file selected");
            }
        }

        match &info.status {
            TorrentStatus::Downloaded => {
                let Some(file_id) = chosen else {
                    return Err(failed(Fail::Unfit, "finished without a file selection"));
                };
                files::verify(&info, hash, file_id, &rules, terms.cap)
                    .map_err(|why| failed(Fail::Unfit, why.to_string()))?;
                tracing::info!(job = job.id, "downloaded and verified");
                return Ok(Report::done(id.to_string(), hash.to_string(), None));
            }
            TorrentStatus::MagnetError
            | TorrentStatus::Error
            | TorrentStatus::Virus
            | TorrentStatus::Dead => {
                return Err(failed(
                    Fail::Refused,
                    format!(
                        "real-debrid reports the torrent as {}",
                        status_name(&info.status)
                    ),
                ));
            }
            _ => {}
        }

        match selected_at {
            None if started.elapsed() >= env.timing.rd_conversion => {
                run_ended(job.id, "timeout");
                return Err(failed(
                    Fail::Timeout,
                    "the magnet did not become a file list in time",
                ));
            }
            Some(at) if at.elapsed() >= terms.wait => {
                run_ended(job.id, "timeout");
                if matches!(
                    info.status,
                    TorrentStatus::Queued | TorrentStatus::Downloading
                ) {
                    // Not cached: it would have to be fetched. Counts against
                    // today's uncached allowance, which is for the worker's
                    // own choices; a human's pick is not counted.
                    if terms.kind == AddKind::Auto
                        && let Err(error) = env.rd.state.record_uncached()
                    {
                        tracing::warn!(%error, "could not count the uncached add");
                    }
                    let progress = Progress {
                        uncached_adds: 1,
                        ..Progress::default()
                    };
                    tell_portal(env, job, progress).await?;
                }
                return Err(failed(Fail::Timeout, "the torrent did not finish in time"));
            }
            _ => {}
        }

        checkpoint(env, job, &mut misses).await?;
        let steps = &env.timing.rd_poll;
        tokio::time::sleep(steps[polls.min(steps.len() - 1)]).await;
        polls += 1;
    }
}

/// Picks the one file to select from the torrent's list and refuses
/// anything unsafe, ambiguous or out of bounds. When a selection is already
/// there (a resumed torrent) it must be exactly that file.
fn settle_files(info: &TorrentInfo, rules: &FileRules, terms: Terms) -> Step<u64> {
    let list = files::files_of(info);
    let file_id = match files::choose(&list, rules) {
        Choice::One(file_id) => file_id,
        Choice::Ambiguous => {
            return Err(failed(
                Fail::Unfit,
                "several videos of similar size; cannot choose",
            ));
        }
        Choice::Unsafe(why) => return Err(failed(Fail::Refused, format!("unsafe torrent: {why}"))),
        Choice::NoVideo => return Err(failed(Fail::Unfit, "the torrent has no video file")),
    };
    if info.files.iter().any(|f| f.selected && f.id != file_id) {
        return Err(failed(
            Fail::Unfit,
            "files other than the chosen one are selected",
        ));
    }
    let bytes = list.iter().find(|f| f.id == file_id).map_or(0, |f| f.bytes);
    if bytes > terms.cap {
        return Err(failed(Fail::Unfit, "the chosen file is over the size cap"));
    }
    Ok(file_id)
}

fn status_name(status: &TorrentStatus) -> &'static str {
    match status {
        TorrentStatus::MagnetError => "magnet_error",
        TorrentStatus::Error => "error",
        TorrentStatus::Virus => "virus",
        TorrentStatus::Dead => "dead",
        _ => "unusable",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::worker::{
        candidate::RawCandidate, rules::JobSpec, score::evaluate, settings::RankSettings,
    };

    const GIB: u64 = 1024 * 1024 * 1024;

    fn raw(letter: char, name: &str, gib: u64) -> RawCandidate {
        RawCandidate {
            infohash: letter.to_string().repeat(40),
            name: name.to_string(),
            filename: None,
            multi_file: false,
            label: None,
            size_bytes: Some(gib * GIB),
            seeders: Some(100),
            provider: None,
            flag_languages: Vec::new(),
            multi_audio: false,
        }
    }

    fn evaluation(pro: bool, candidates: &[RawCandidate]) -> Evaluation {
        let spec = JobSpec {
            title: "Example Movie".into(),
            year: Some(1994),
            infuse_pro: pro,
            blocked: HashSet::new(),
        };
        evaluate(&spec, candidates, &RankSettings::default())
    }

    const PLAYS: &str = "Example.Movie.1994.1080p.BluRay.x264.AAC";
    const UNTAGGED: &str = "Example.Movie.1994.1080p.BluRay.x264";

    #[test]
    fn only_the_planned_candidates_are_eligible_in_their_planned_order() {
        let eval = evaluation(
            false,
            &[
                raw('a', UNTAGGED, 8),
                raw('b', PLAYS, 9),
                raw('c', PLAYS, 10),
            ],
        );
        let cap = 15 * GIB;
        assert_eq!(
            eligible_hashes(&eval, false, cap),
            ["b".repeat(40), "c".repeat(40)]
        );
    }

    #[test]
    fn a_free_job_never_gets_a_hash_without_a_yes_badge_even_if_the_plan_listed_it() {
        // The ranking is tampered with, as a bug in it might: the untagged
        // release is given an auto rank and a place in the list.
        let mut eval = evaluation(false, &[raw('a', UNTAGGED, 8)]);
        assert_eq!(eval.shortlist[0].badge, FreeBadge::Unknown);
        assert!(eligible_hashes(&eval, false, 15 * GIB).is_empty());
        eval.shortlist[0].auto_rank = 1;
        eval.auto.push("a".repeat(40));
        assert!(eligible_hashes(&eval, false, 15 * GIB).is_empty());
        assert_eq!(eligible_hashes(&eval, true, 15 * GIB), ["a".repeat(40)]);
    }

    #[test]
    fn a_hash_outside_the_ranking_list_or_over_the_cap_or_malformed_is_not_eligible() {
        let mut eval = evaluation(false, &[raw('a', PLAYS, 8), raw('b', PLAYS, 12)]);
        assert_eq!(eligible_hashes(&eval, false, 15 * GIB).len(), 2);
        assert_eq!(eligible_hashes(&eval, false, 10 * GIB), ["a".repeat(40)]);
        eval.auto.retain(|hash| *hash != "a".repeat(40));
        assert_eq!(eligible_hashes(&eval, false, 15 * GIB), ["b".repeat(40)]);
        eval.shortlist[1].raw.infohash = "zz".repeat(20);
        eval.auto = vec!["zz".repeat(20)];
        assert!(eligible_hashes(&eval, false, 15 * GIB).is_empty());
        eval.shortlist[1].raw.size_bytes = None;
        assert!(eligible_hashes(&eval, false, u64::MAX).is_empty());
    }
}
