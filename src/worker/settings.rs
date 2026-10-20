//! The numbers the worker judges candidates by. One struct with a
//! `Default`, so tuning from the dry-run data means changing values here
//! (or, for a few, from the environment in the runtime).
//!
//! The weights are starting guesses, to be tuned once the dry-run logs show
//! how often the top pick matches a human's.

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Points for the video height. 1080p leads on purpose: smaller files, plays
/// everywhere and is kinder to the WebDAV mount and the Pi than 2160p. Flip
/// it by changing these numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolutionPoints {
    pub r2160: f64,
    pub r1440: f64,
    pub r1080: f64,
    pub r720: f64,
    pub sd: f64,
    pub unknown: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourcePoints {
    pub bluray: f64,
    pub web_dl: f64,
    pub remux: f64,
    pub web_rip: f64,
    pub hdtv: f64,
    pub hdrip: f64,
    pub dvd: f64,
    pub unknown: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodecPoints {
    pub h264: f64,
    pub h265: f64,
    pub av1: f64,
    pub other: f64,
}

/// The file size, in GiB, that earns the full size points for a height.
/// Outside it the points fall linearly to zero at half the lower edge and at
/// twice the upper edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweetSpot {
    pub low_gib: f64,
    pub high_gib: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankSettings {
    // Hard filters.
    /// A torrent smaller than this is dropped.
    pub min_size_bytes: u64,
    /// A torrent larger than this is dropped.
    pub max_size_bytes: u64,
    /// Languages that count as the wanted audio (ISO 639-1). A release whose
    /// language signals are all outside this list is penalised, not dropped.
    pub allowed_languages: Vec<String>,

    // Scoring.
    pub resolution: ResolutionPoints,
    pub source: SourcePoints,
    pub codec: CodecPoints,
    /// The most points the size can earn.
    pub size_max_points: f64,
    pub sweet_720: SweetSpot,
    pub sweet_1080: SweetSpot,
    pub sweet_1440: SweetSpot,
    pub sweet_2160: SweetSpot,
    pub sweet_sd: SweetSpot,
    /// Seeders earn `seeder_factor * log10(1 + seeders)`, at most this many.
    pub seeder_max_points: f64,
    pub seeder_factor: f64,
    /// Subtracted when the language signals say a likely dub.
    pub foreign_language_penalty: f64,
    /// Added for a free job's candidate whose badge is `yes`, so a verified
    /// release beats an unverified one a few points ahead. PRO jobs get none.
    pub free_badge_bonus: f64,

    // The shortlist and the clear-winner rule.
    pub shortlist_len: usize,
    /// The top score must reach this to be a clear winner.
    pub clear_min_score: f64,
    /// The top score must lead the runner-up by this much.
    pub clear_margin: f64,
    /// Two otherwise identical candidates whose sizes differ by at most this
    /// fraction of the better one's are one release; the worse one folds in.
    pub dup_size_tolerance: f64,

    // The auto pick rule: which candidates an automatic job may try, in order.
    /// A candidate must score at least this.
    pub auto_min_score: f64,
    /// A candidate must have at least this many seeders, a weak proxy for
    /// being cached at Real-Debrid.
    pub auto_min_seeders: u32,
    /// How many candidates an auto job tries at most.
    pub auto_max_tries: usize,
    /// A candidate must have a known size of at most this, free and PRO alike.
    pub auto_max_bytes: u64,
}

impl Default for RankSettings {
    fn default() -> Self {
        Self {
            min_size_bytes: 400 * MIB,
            max_size_bytes: 40 * GIB,
            allowed_languages: vec!["en".to_string()],
            resolution: ResolutionPoints {
                r2160: 32.0,
                r1440: 20.0,
                r1080: 40.0,
                r720: 22.0,
                sd: 6.0,
                unknown: 0.0,
            },
            source: SourcePoints {
                bluray: 30.0,
                web_dl: 28.0,
                remux: 24.0,
                web_rip: 20.0,
                hdtv: 10.0,
                hdrip: 8.0,
                dvd: 5.0,
                unknown: 6.0,
            },
            codec: CodecPoints {
                h264: 10.0,
                h265: 10.0,
                av1: 4.0,
                other: 0.0,
            },
            size_max_points: 15.0,
            sweet_720: SweetSpot {
                low_gib: 1.0,
                high_gib: 4.0,
            },
            sweet_1080: SweetSpot {
                low_gib: 2.0,
                high_gib: 12.0,
            },
            sweet_1440: SweetSpot {
                low_gib: 4.0,
                high_gib: 20.0,
            },
            sweet_2160: SweetSpot {
                low_gib: 10.0,
                high_gib: 40.0,
            },
            sweet_sd: SweetSpot {
                low_gib: 0.5,
                high_gib: 2.0,
            },
            seeder_max_points: 20.0,
            seeder_factor: 7.0,
            foreign_language_penalty: 25.0,
            free_badge_bonus: 6.0,
            shortlist_len: 10,
            clear_min_score: 70.0,
            clear_margin: 8.0,
            dup_size_tolerance: 0.01,
            auto_min_score: 85.0,
            auto_min_seeders: 10,
            auto_max_tries: 3,
            auto_max_bytes: 15 * GIB,
        }
    }
}
