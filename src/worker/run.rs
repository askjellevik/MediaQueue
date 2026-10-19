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

use thiserror::Error;
use tokio::sync::watch;

use super::{
    backoff::Backoff,
    candidate::RawCandidate,
    portal::{Abandon, Job, Mode, PortalClient, PortalError, Report, ReportResult},
    rules::{DropReason, JobSpec},
    score::{Candidate, Evaluation, evaluate},
    settings::RankSettings,
    source::{CandidateSource, MediaKind, SourceError},
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
}

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
    idle: Option<Idle>,
    first_call: bool,
    stats: Stats,
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
            idle: None,
            first_call: true,
            stats: Stats::default(),
        }
    }

    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
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
        let idle = if !settings.enabled {
            Some(Idle::KillSwitch)
        } else if settings.mode != Mode::DryRun {
            Some(Idle::Mode(settings.mode))
        } else {
            None
        };
        if let Some(reason) = idle {
            if self.idle != Some(reason) {
                match reason {
                    Idle::KillSwitch => tracing::info!("idle: kill switch off"),
                    Idle::Mode(mode) => tracing::info!(
                        "idle: mode {} needs the Real-Debrid worker",
                        mode_name(mode)
                    ),
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
                self.work(job, now).await;
                Ok(Cycle::Worked)
            }
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
    async fn work(&mut self, job: Job, now: Instant) {
        let mut stop = self.shutdown.clone();
        let run = async {
            let report = self.report_for(&job, now).await;
            self.deliver(&job, &report).await;
        };
        tokio::select! {
            () = run => {}
            () = async {
                stop.wait().await;
                tokio::time::sleep(SHUTDOWN_GRACE).await;
            } => tracing::warn!(job = job.id, "shutdown: run dropped, the portal will reclaim the job"),
        }
    }

    async fn report_for(&mut self, job: &Job, now: Instant) -> Report {
        match tokio::time::timeout(RUN_DEADLINE, self.plan(job, now)).await {
            Ok(report) => report,
            Err(_) => {
                self.stats.errors += 1;
                tracing::warn!(job = job.id, "run exceeded 5 minutes");
                Report::failed(Some("timeout"), "run exceeded 5 minutes")
            }
        }
    }

    /// What to tell the portal about this job.
    async fn plan(&mut self, job: &Job, now: Instant) -> Report {
        if job.media_type == "tv" {
            return Report::failed(None, "series are not supported by the worker yet");
        }
        if !is_imdb_id(&job.imdb_id) {
            return Report::failed(None, "invalid imdb id from the portal");
        }
        if let Some(hash) = &job.chosen_infohash {
            tracing::info!(job = job.id, "would add {hash} (picked: {})", job.picked);
            return Report {
                chosen_infohash: Some(hash.clone()),
                ..bare_needs_pick("dry_run")
            };
        }

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
                return Report::failed(Some("source_unreachable"), &text);
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

        if raw.is_empty() {
            bare_needs_pick("no_candidates")
        } else if evaluation.shortlist.is_empty() {
            bare_needs_pick("no_safe_candidate")
        } else {
            Report::needs_pick(
                evaluation
                    .shortlist
                    .iter()
                    .map(Candidate::to_report_map)
                    .collect(),
                evaluation.top_infohash,
                "dry_run",
            )
        }
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
) -> Result<(), WorkerError> {
    let mut worker = Worker::new(portal, source, rank, poll, shutdown);
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
        foreign_language = evaluation.foreign_language,
        foreign_in_shortlist = evaluation.foreign_in_shortlist,
        shortlist = evaluation.shortlist.len(),
        top_score = top.map(|c| c.score),
        margin = evaluation.margin,
        clear_winner = evaluation.clear_winner,
        top_badge = top.map_or("-", |c| c.badge.as_str()),
        top_name = %top_name,
        "dry run"
    );
}
