//! The dry-run loop: ask the portal whether to work, claim a job, look the
//! title up at the candidate source, rank what came back and report the
//! shortlist for a person to pick from. Nothing is added anywhere.
//!
//! One cycle is [`Worker::run_once`], so tests drive it without sleeping;
//! [`run`] is the loop around it.

use std::{
    collections::{BTreeMap, HashSet},
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::sync::watch;

use super::{
    backoff::Backoff,
    candidate::RawCandidate,
    fetch::{self, Env, Outcome, Trace},
    portal::{
        Abandon, AlertKind, Job, Mode, PortalClient, PortalError, Report, ReportResult, ReportState,
    },
    rd::{self, RdRuntime},
    rules::{DropReason, JobSpec},
    score::{Evaluation, evaluate},
    settings::RankSettings,
    source::{CandidateSource, MediaKind, SourceError},
    state::Trip,
};

/// A run that takes longer is reported as a timeout. Well inside the
/// portal's 45 minutes before it hands a claimed job to someone else.
const RUN_DEADLINE: Duration = Duration::from_secs(5 * 60);
/// How long a run in flight gets after a shutdown was asked for.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(8);
/// Between two jobs when the queue is being drained.
const JOB_GAP: Duration = Duration::from_secs(1);
const PORTAL_BACKOFF_CAP: Duration = Duration::from_secs(60);
const UNAUTHORIZED_WAIT: Duration = Duration::from_secs(60);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(60 * 60);
const TOP_NAME_CHARS: usize = 120;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("the portal refused INTERNAL_TOKEN (401); it must match the portal's")]
    Unauthorized,
}

/// The waits inside a run. [`Timing::default`] is what runs for real; tests
/// shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Before the second and third try at an unreachable source.
    pub source_retries: [Duration; 2],
    /// Before each new try at a report the portal could not take.
    pub report_retries: [Duration; 3],
    /// Between two looks at a torrent at Real-Debrid; the last one repeats.
    pub rd_poll: [Duration; 7],
    /// How long a magnet may take to turn into a file list.
    pub rd_conversion: Duration,
    /// How long a picked torrent may take after its file was selected.
    pub rd_picked_wait: Duration,
    /// The whole run of a picked job, well under the portal's reclaim.
    pub rd_picked_deadline: Duration,
    /// How long a candidate the worker chose may take after its file was
    /// selected: a cached torrent is ready within seconds.
    pub rd_auto_wait: Duration,
    /// The whole run of an auto job, all its candidates together.
    pub rd_auto_deadline: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            source_retries: [Duration::from_secs(2), Duration::from_secs(10)],
            report_retries: [
                Duration::from_secs(2),
                Duration::from_secs(10),
                Duration::from_secs(30),
            ],
            rd_poll: [3, 3, 5, 5, 10, 10, 15].map(Duration::from_secs),
            rd_conversion: Duration::from_secs(60),
            rd_picked_wait: Duration::from_secs(10 * 60),
            rd_picked_deadline: Duration::from_secs(15 * 60),
            rd_auto_wait: Duration::from_secs(90),
            rd_auto_deadline: Duration::from_secs(8 * 60),
        }
    }
}

/// Resolves waits early once a stop was asked for. A dropped sender counts
/// as a stop.
#[derive(Clone)]
pub struct Shutdown {
    stop: watch::Receiver<bool>,
}

impl Shutdown {
    pub fn channel() -> (watch::Sender<bool>, Self) {
        let (tx, rx) = watch::channel(false);
        (tx, Self { stop: rx })
    }

    pub fn requested(&self) -> bool {
        *self.stop.borrow()
    }

    async fn wait(&mut self) {
        let _ = self.stop.wait_for(|stop| *stop).await;
    }

    /// Sleeps; true when a stop cut it short or came during it.
    async fn sleep(&mut self, duration: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(duration) => self.requested(),
            _ = self.wait() => true,
        }
    }
}

/// What the loop does after a cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cycle {
    /// A job was claimed and settled (or given up on).
    Worked,
    /// Nothing was claimed; try again after this long.
    Wait(Duration),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Idle {
    KillSwitch,
    Mode(Mode),
    /// Real-Debrid is being left alone, or the counters cannot be read.
    Hold(&'static str),
}

/// What has already been announced, so one cause raises one alert.
#[derive(Default)]
struct Alerted {
    auth: bool,
    unreadable: bool,
    /// The end of the breaker period the last alert was for.
    breaker_until: Option<u64>,
    /// Real-Debrid runs in a row that ended without a result.
    failed_runs: u32,
}

/// The run of failures that raises `repeated_failures`.
const REPEATED_FAILURES: u32 = 3;

#[derive(Default)]
struct Stats {
    polls: u64,
    claims: u64,
    reports: u64,
    errors: u64,
}

pub struct Worker<S> {
    portal: PortalClient,
    source: S,
    rank: RankSettings,
    poll: Duration,
    shutdown: Shutdown,
    timing: Timing,
    source_backoff: Backoff,
    /// Present when `RD_TOKEN_FILE` is configured.
    rd: Option<RdRuntime>,
    idle: Option<Idle>,
    first_call: bool,
    stats: Stats,
    alerted: Alerted,
}

impl<S: CandidateSource> Worker<S> {
    pub fn new(
        portal: PortalClient,
        source: S,
        rank: RankSettings,
        poll: Duration,
        shutdown: Shutdown,
    ) -> Self {
        Self {
            portal,
            source,
            rank,
            poll,
            shutdown,
            timing: Timing::default(),
            source_backoff: Backoff::for_source(),
            rd: None,
            idle: None,
            first_call: true,
            stats: Stats::default(),
            alerted: Alerted::default(),
        }
    }

    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    pub fn with_rd(mut self, rd: Option<RdRuntime>) -> Self {
        self.rd = rd;
        self
    }

    /// One pass: stay away while the source is in backoff, check the
    /// settings, claim at most one job and run it. `now` is only used for
    /// the source's backoff.
    pub async fn run_once(&mut self, now: Instant) -> Result<Cycle, WorkerError> {
        if let Some(left) = self.source_backoff.remaining(now) {
            return Ok(Cycle::Wait(left));
        }

        let first = std::mem::replace(&mut self.first_call, false);
        self.stats.polls += 1;
        let settings = match self.portal.settings().await {
            Ok(settings) => settings,
            Err(e) => return self.portal_failed(&e, first),
        };
        // Only where the Real-Debrid worker is switched on: a dry-run worker,
        // or one that is switched off, makes no Real-Debrid call at all.
        if let Some(rd) = &self.rd
            && settings.enabled
            && settings.mode != Mode::DryRun
            && rd.state.has_pending_delete()
        {
            fetch::sweep_pending(rd).await;
        }
        self.raise_alerts().await;

        let rd_mode = settings.mode != Mode::DryRun;
        let idle = if !settings.enabled {
            Some(Idle::KillSwitch)
        } else if rd_mode && self.rd.is_none() {
            Some(Idle::Mode(settings.mode))
        } else {
            self.rd_hold(rd_mode)
        };
        if let Some(reason) = idle {
            if self.idle != Some(reason) {
                match reason {
                    Idle::KillSwitch => tracing::info!("idle: kill switch off"),
                    Idle::Mode(mode) => tracing::info!(
                        "idle: mode {} needs the Real-Debrid worker",
                        mode_name(mode)
                    ),
                    Idle::Hold(why) => tracing::info!("idle: {why}"),
                }
            }
            self.idle = idle;
            return Ok(Cycle::Wait(self.poll));
        }
        self.idle = None;

        match self.portal.claim().await {
            Err(e) => self.portal_failed(&e, false),
            Ok(None) => Ok(Cycle::Wait(self.poll)),
            Ok(Some(job)) => {
                self.stats.claims += 1;
                self.work(job, now, settings.mode).await;
                self.raise_alerts().await;
                Ok(Cycle::Worked)
            }
        }
    }

    /// Tells the portal about a cause that needs a person, once per cause:
    /// a new breaker period, a bad token, a counters file that cannot be
    /// trusted. A failed call is logged and changes nothing else.
    async fn raise_alerts(&mut self) {
        let Some(rd) = &self.rd else {
            return;
        };
        let now = rd.state.now();
        let mut kinds = Vec::new();
        match rd.state.breaker_trip(now) {
            Some(Trip::Auth) if !self.alerted.auth => {
                self.alerted.auth = true;
                kinds.push(AlertKind::RdAuth);
            }
            Some(Trip::RateLimit | Trip::Forbidden) => {
                let until = rd.state.breaker_open(now).map(|left| now + left.as_secs());
                if until.is_some() && until != self.alerted.breaker_until {
                    self.alerted.breaker_until = until;
                    kinds.push(AlertKind::BreakerOpen);
                }
            }
            _ => {}
        }
        if rd.state.unreadable() && !self.alerted.unreadable {
            self.alerted.unreadable = true;
            kinds.push(AlertKind::CountersUnreadable);
        }
        for kind in kinds {
            self.send_alert(kind).await;
        }
    }

    async fn send_alert(&self, kind: AlertKind) {
        if let Err(error) = self.portal.alert(kind).await {
            tracing::warn!(?kind, %error, "alert not delivered");
        }
    }

    /// Counts a finished Real-Debrid run; the third failure in a row raises
    /// an alert, a result starts the count over.
    async fn note_rd_run(&mut self, report: &Report) {
        if report.state == ReportState::Done {
            self.alerted.failed_runs = 0;
            return;
        }
        self.alerted.failed_runs += 1;
        if self.alerted.failed_runs == REPEATED_FAILURES {
            self.send_alert(AlertKind::RepeatedFailures).await;
        }
    }

    /// In the Real-Debrid modes: no job is claimed while the breaker is
    /// open or the counters cannot be read. The daily caps are not checked
    /// here: they stop only the adds the worker chooses itself, and a human's
    /// pick must still be claimed.
    fn rd_hold(&self, rd_mode: bool) -> Option<Idle> {
        let rd = self.rd.as_ref().filter(|_| rd_mode)?;
        if rd.state.breaker_trip(rd.state.now()).is_some() {
            Some(Idle::Hold("real-debrid is being left alone (breaker open)"))
        } else if rd.state.unreadable() {
            Some(Idle::Hold("the counters file cannot be trusted"))
        } else {
            None
        }
    }

    fn portal_failed(&mut self, error: &PortalError, first: bool) -> Result<Cycle, WorkerError> {
        self.stats.errors += 1;
        if is_unauthorized(error) {
            if first {
                return Err(WorkerError::Unauthorized);
            }
            tracing::error!(%error, "the portal refused the token; check INTERNAL_TOKEN");
            return Ok(Cycle::Wait(UNAUTHORIZED_WAIT));
        }
        match error {
            PortalError::Transient(_) => tracing::warn!(%error, "portal unavailable"),
            PortalError::Fatal(_) => tracing::error!(%error, "portal call failed"),
        }
        Ok(Cycle::Wait((self.poll * 2).min(PORTAL_BACKOFF_CAP)))
    }

    /// Runs and reports one job. A stop asked for meanwhile gives it a few
    /// seconds, after which it is dropped and the portal reclaims the job.
    async fn work(&mut self, job: Job, now: Instant, mode: Mode) {
        let mut stop = self.shutdown.clone();
        let run = rd::within_job(job.id, async {
            let report = if mode != Mode::DryRun && self.rd.is_some() {
                self.report_rd(&job, now, mode).await
            } else {
                Some(self.report_for(&job, now, mode).await)
            };
            if let Some(report) = report {
                self.deliver(&job, &report).await;
            }
        });
        tokio::select! {
            () = run => {}
            () = async {
                stop.wait().await;
                tokio::time::sleep(SHUTDOWN_GRACE).await;
            } => tracing::warn!(job = job.id, "shutdown: run dropped, the portal will reclaim the job"),
        }
    }

    async fn report_for(&mut self, job: &Job, now: Instant, mode: Mode) -> Report {
        match tokio::time::timeout(RUN_DEADLINE, self.plan(job, now, mode)).await {
            Ok(report) => report,
            Err(_) => {
                self.stats.errors += 1;
                tracing::warn!(job = job.id, "run exceeded 5 minutes");
                fetch::run_ended(job.id, "timeout");
                Report::failed(Some("timeout"), "run exceeded 5 minutes")
            }
        }
    }

    /// What to tell the portal about this job.
    async fn plan(&mut self, job: &Job, now: Instant, mode: Mode) -> Report {
        if let Some(report) = refuse_unsupported(job) {
            return report;
        }
        let dry_run = mode == Mode::DryRun;
        if dry_run && let Some(hash) = &job.chosen_infohash {
            tracing::info!(job = job.id, "would add {hash} (picked: {})", job.picked);
            return Report {
                chosen_infohash: Some(hash.clone()),
                ..bare_needs_pick("dry_run")
            };
        }
        // Nothing picked: the shortlist goes to the admin. In dry-run that
        // is the whole point; in assisted mode it is where the job waits for
        // a person.
        let pick_reason = if dry_run {
            "dry_run"
        } else {
            "no_clear_choice"
        };
        let evaluation = match self.search(job, now).await {
            Ok(Searched::Nothing(reason)) => return bare_needs_pick(reason),
            Ok(Searched::Ranked(evaluation)) => evaluation,
            Err(report) => return report,
        };
        Report::needs_pick(
            shortlist_maps(&evaluation),
            evaluation.top_infohash,
            pick_reason,
        )
    }

    /// Asks the source for the title and ranks what came back.
    async fn search(&mut self, job: &Job, now: Instant) -> Result<Searched, Report> {
        let began = Instant::now();
        let raw = match self.fetch(&job.imdb_id).await {
            Ok(raw) => {
                self.source_backoff.success();
                raw
            }
            Err(e) => {
                self.stats.errors += 1;
                let wait = self.source_backoff.failure(now + began.elapsed());
                tracing::warn!(
                    job = job.id,
                    error = %e,
                    backoff_secs = wait.as_secs(),
                    "candidate source failed"
                );
                let text = match e {
                    SourceError::Unreachable(text) => {
                        format!("candidate source unreachable: {text}")
                    }
                    SourceError::BadResponse(text) => {
                        format!("candidate source sent an unusable response: {text}")
                    }
                };
                return Err(Report::failed(Some("source_unreachable"), &text));
            }
        };

        let spec = JobSpec {
            title: job.title.clone(),
            year: job.year.and_then(|year| i32::try_from(year).ok()),
            infuse_pro: job.infuse_pro,
            blocked: job
                .blocked_hashes
                .iter()
                .map(|hash| hash.to_ascii_lowercase())
                .collect::<HashSet<_>>(),
        };
        let evaluation = evaluate(&spec, &raw, &self.rank);
        log_run(job, &evaluation);

        Ok(if raw.is_empty() {
            Searched::Nothing("no_candidates")
        } else if evaluation.shortlist.is_empty() {
            Searched::Nothing("no_safe_candidate")
        } else {
            Searched::Ranked(evaluation)
        })
    }

    /// The Real-Debrid modes. Returns `None` when the claim is gone and
    /// there is nothing to report.
    async fn report_rd(&mut self, job: &Job, now: Instant, mode: Mode) -> Option<Report> {
        if let Some(report) = refuse_unsupported(job) {
            return Some(report);
        }
        let Some(rd) = &self.rd else {
            return Some(Report::failed(None, "no real-debrid runtime"));
        };
        let env = Env {
            rd,
            portal: &self.portal,
            timing: &self.timing,
            auto_max_bytes: self.rank.auto_max_bytes,
        };
        fetch::handle_removal(&env, job).await;

        if !(job.picked && job.chosen_infohash.is_some()) {
            if mode == Mode::Auto {
                return self.report_auto(job, now).await;
            }
            return Some(self.report_for(job, now, Mode::Assisted).await);
        }

        let mut trace = Trace::default();
        let deadline = self.timing.rd_picked_deadline;
        let report =
            match tokio::time::timeout(deadline, fetch::picked(&env, job, &mut trace)).await {
                Ok(Outcome::Report(report)) => Some(report),
                Ok(Outcome::Dropped) => None,
                Err(_) => {
                    fetch::cleanup(&env, &mut trace, None).await;
                    self.stats.errors += 1;
                    tracing::warn!(job = job.id, "picked run exceeded its deadline");
                    fetch::run_ended(job.id, "timeout");
                    Some(Report::needs_pick_with(
                        "picked_failed",
                        "the run took too long",
                    ))
                }
            };
        if let Some(report) = &report {
            self.note_rd_run(report).await;
        }
        report
    }

    /// An unpicked job in auto mode: search, rank, then let the auto rule
    /// walk the planned candidates. A chosen hash the job may carry from an
    /// earlier run is a hint at most; the plan decides. Every report that
    /// sends the job back to the admin carries the shortlist.
    async fn report_auto(&mut self, job: &Job, now: Instant) -> Option<Report> {
        let searched = match tokio::time::timeout(RUN_DEADLINE, self.search(job, now)).await {
            Ok(Ok(searched)) => searched,
            Ok(Err(report)) => return Some(report),
            Err(_) => {
                self.stats.errors += 1;
                tracing::warn!(job = job.id, "search exceeded 5 minutes");
                fetch::run_ended(job.id, "timeout");
                return Some(Report::failed(Some("timeout"), "run exceeded 5 minutes"));
            }
        };
        let evaluation = match searched {
            Searched::Nothing(reason) => return Some(bare_needs_pick(reason)),
            Searched::Ranked(evaluation) => evaluation,
        };
        let Some(rd) = &self.rd else {
            return Some(Report::failed(None, "no real-debrid runtime"));
        };
        let env = Env {
            rd,
            portal: &self.portal,
            timing: &self.timing,
            auto_max_bytes: self.rank.auto_max_bytes,
        };
        let mut trace = Trace {
            candidates: shortlist_maps(&evaluation),
            top_infohash: evaluation.top_infohash.clone(),
            ..Trace::default()
        };
        let deadline = self.timing.rd_auto_deadline;
        let mut report =
            match tokio::time::timeout(deadline, fetch::auto(&env, job, &evaluation, &mut trace))
                .await
            {
                Ok(Outcome::Report(report)) => report,
                Ok(Outcome::Dropped) => return None,
                Err(_) => {
                    fetch::cleanup(&env, &mut trace, None).await;
                    self.stats.errors += 1;
                    tracing::warn!(job = job.id, "auto run exceeded its deadline");
                    fetch::run_ended(job.id, "timeout");
                    Report::needs_pick_with("rd_timeout", "the run took too long")
                }
            };
        if report.state == ReportState::NeedsPick && report.candidates.is_none() {
            report.candidates = Some(std::mem::take(&mut trace.candidates));
            report.top_infohash = trace.top_infohash.take();
        }
        // Nothing eligible never reached Real-Debrid: not a failed run.
        if report.reason != Some("no_clear_choice") {
            self.note_rd_run(&report).await;
        }
        Some(report)
    }

    /// Asks the source, trying an unreachable one again after short pauses.
    /// A bad answer is not tried again; a stop skips the pauses.
    async fn fetch(&mut self, imdb_id: &str) -> Result<Vec<RawCandidate>, SourceError> {
        let pauses = self.timing.source_retries;
        let mut retries = pauses.into_iter();
        loop {
            let result = self.source.candidates(imdb_id, MediaKind::Movie).await;
            if !matches!(result, Err(SourceError::Unreachable(_))) {
                return result;
            }
            let Some(pause) = retries.next() else {
                return result;
            };
            if self.shutdown.sleep(pause).await {
                return result;
            }
        }
    }

    /// Sends the report, trying again when the portal could not take it.
    async fn deliver(&mut self, job: &Job, report: &Report) {
        let pauses = self.timing.report_retries;
        let mut retries = pauses.into_iter();
        loop {
            match self.portal.report(job.id, &job.claim_id, report).await {
                Ok(ReportResult::Applied) => {
                    self.stats.reports += 1;
                    return;
                }
                Ok(ReportResult::Abandon(why)) => {
                    tracing::info!(
                        job = job.id,
                        status = abandon_name(&why),
                        "claim no longer valid, run abandoned"
                    );
                    return;
                }
                Err(error @ PortalError::Transient(_)) => {
                    self.stats.errors += 1;
                    let Some(pause) = retries.next() else {
                        tracing::error!(job = job.id, %error, "report not delivered, giving up");
                        return;
                    };
                    tracing::warn!(job = job.id, %error, "report not delivered, trying again");
                    if self.shutdown.sleep(pause).await {
                        return;
                    }
                }
                Err(error @ PortalError::Fatal(_)) => {
                    // The portal's own rules turned the report down: a bug
                    // here, and trying again would not change it.
                    self.stats.errors += 1;
                    tracing::error!(job = job.id, %error, "the portal rejected the report");
                    return;
                }
            }
        }
    }

    fn heartbeat(&self) {
        tracing::info!(
            polls = self.stats.polls,
            claims = self.stats.claims,
            reports = self.stats.reports,
            errors = self.stats.errors,
            "worker heartbeat"
        );
    }
}

/// The loop, until a stop is asked for. Fails only when the portal refuses
/// the token on the very first call.
pub async fn run<S: CandidateSource>(
    portal: PortalClient,
    source: S,
    rank: RankSettings,
    poll: Duration,
    shutdown: Shutdown,
    rd: Option<RdRuntime>,
) -> Result<(), WorkerError> {
    let mut worker = Worker::new(portal, source, rank, poll, shutdown).with_rd(rd);
    let mut last_heartbeat = Instant::now();
    while !worker.shutdown.requested() {
        let now = Instant::now();
        if now.duration_since(last_heartbeat) >= HEARTBEAT_EVERY {
            worker.heartbeat();
            last_heartbeat = now;
        }
        let pause = match worker.run_once(now).await? {
            Cycle::Worked => JOB_GAP,
            Cycle::Wait(wait) => wait,
        };
        if worker.shutdown.sleep(pause).await {
            break;
        }
    }
    tracing::info!("worker stopped");
    Ok(())
}

/// A job no mode can run: a series, or an IMDb id that is not one.
fn refuse_unsupported(job: &Job) -> Option<Report> {
    if job.media_type == "tv" {
        return Some(Report::failed(
            None,
            "series are not supported by the worker yet",
        ));
    }
    if !is_imdb_id(&job.imdb_id) {
        return Some(Report::failed(None, "invalid imdb id from the portal"));
    }
    None
}

/// What a search found.
enum Searched {
    /// The source offered nothing, or nothing survived the filters.
    Nothing(&'static str),
    Ranked(Evaluation),
}

/// The shortlist as the portal takes it, each entry with the survivor count.
fn shortlist_maps(evaluation: &Evaluation) -> Vec<Map<String, Value>> {
    let survivors = json!(evaluation.survivors);
    evaluation
        .shortlist
        .iter()
        .map(|candidate| {
            let mut map = candidate.to_report_map();
            map.insert("survivors".into(), survivors.clone());
            map
        })
        .collect()
}

/// `needs_pick` with no candidates sent, so the portal keeps any it holds.
fn bare_needs_pick(reason: &'static str) -> Report {
    Report {
        candidates: None,
        ..Report::needs_pick(Vec::new(), None, reason)
    }
}

/// The portal's 401 has no variant of its own; its text starts with the status.
fn is_unauthorized(error: &PortalError) -> bool {
    matches!(error, PortalError::Fatal(text) if text.starts_with("status 401"))
}

fn is_imdb_id(id: &str) -> bool {
    id.strip_prefix("tt").is_some_and(|digits| {
        (5..=10).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::DryRun => "dry_run",
        Mode::Assisted => "assisted",
        Mode::Auto => "auto",
    }
}

fn abandon_name(why: &Abandon) -> &'static str {
    match why {
        Abandon::StaleClaim { .. } => "stale_claim",
        Abandon::NotFound => "not_found",
        Abandon::PickedLocked => "picked_locked",
    }
}

fn drop_counts(dropped: &BTreeMap<DropReason, usize>) -> String {
    if dropped.is_empty() {
        return "none".to_string();
    }
    dropped
        .iter()
        .map(|(reason, n)| format!("{}={n}", reason.as_str()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The dry-run metrics, one line per run.
fn log_run(job: &Job, evaluation: &Evaluation) {
    let top = evaluation.shortlist.first();
    let top_name: String = top
        .map(|c| c.raw.name.chars().take(TOP_NAME_CHARS).collect())
        .unwrap_or_default();
    tracing::info!(
        job = job.id,
        imdb_id = %job.imdb_id,
        profile = if job.infuse_pro { "pro" } else { "free" },
        received = evaluation.considered,
        dropped = %drop_counts(&evaluation.dropped),
        title_mismatch = evaluation
            .dropped
            .get(&DropReason::TitleMismatch)
            .copied()
            .unwrap_or(0),
        survivors = evaluation.survivors,
        collapsed = evaluation.collapsed,
        foreign_language = evaluation.foreign_language,
        foreign_in_shortlist = evaluation.foreign_in_shortlist,
        shortlist = evaluation.shortlist.len(),
        top_score = top.map(|c| c.score),
        margin = evaluation.margin,
        clear_winner = evaluation.clear_winner,
        auto = evaluation.auto.len(),
        auto_top_badge = evaluation
            .shortlist
            .iter()
            .find(|c| c.auto_rank == 1)
            .map_or("-", |c| c.badge.as_str()),
        top_badge = top.map_or("-", |c| c.badge.as_str()),
        top_name = %top_name,
        "dry run"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::RECLAIM_AFTER_SECS;

    #[test]
    fn a_healthy_picked_run_ends_long_before_the_portal_reclaims_the_job() {
        let timing = Timing::default();
        let reclaim = Duration::from_secs(u64::try_from(RECLAIM_AFTER_SECS).unwrap());
        let one_candidate = timing.rd_conversion + timing.rd_picked_wait;
        assert!(one_candidate < reclaim, "{one_candidate:?}");
        assert!(timing.rd_picked_deadline < reclaim);
        // The deadline leaves room for conversion and the wait, or it would
        // cut off a run that is behaving.
        assert!(timing.rd_picked_deadline > one_candidate);
        // The dry run has its own, shorter limit.
        assert!(RUN_DEADLINE < reclaim);
    }

    #[test]
    fn an_auto_run_fits_all_its_tries_before_its_deadline_and_the_reclaim() {
        let timing = Timing::default();
        let tries = u32::try_from(RankSettings::default().auto_max_tries).unwrap();
        let all_tries = (timing.rd_conversion + timing.rd_auto_wait) * tries;
        assert!(all_tries < timing.rd_auto_deadline, "{all_tries:?}");
        let reclaim = Duration::from_secs(u64::try_from(RECLAIM_AFTER_SECS).unwrap());
        assert!(timing.rd_auto_deadline < reclaim);
        // A cached torrent is ready long before a human's pick would be given up on.
        assert!(timing.rd_auto_wait < timing.rd_picked_wait);
    }
}
