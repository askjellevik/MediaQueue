//! The numbers the worker judges candidates by. One struct with a
//! `Default`, so tuning from the dry-run data means changing values here
//! (or, for a few, from the environment in the runtime).
//!
//! The weights are starting guesses, to be tuned once the dry-run logs show
//! how often the top pick matches a human's.

use super::rd_terms::{self, BlockedTerm};

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

/// The weights of the PRO ranking (see `score::score_pro`): the best release
/// possible, not the smallest. Bigger files are better, since a PRO release
/// is streamed through Zurg, not downloaded. Code-only: no environment
/// variable changes them, except the preferred groups in `RankSettings`.
#[derive(Debug, Clone, PartialEq)]
pub struct ProPoints {
    pub r2160: f64,
    pub r1440: f64,
    pub r1080: f64,
    pub r720: f64,
    pub sd: f64,
    pub resolution_unknown: f64,
    pub remux: f64,
    pub bluray: f64,
    pub web_dl: f64,
    pub web_rip: f64,
    pub hdtv: f64,
    pub hdrip: f64,
    pub dvd: f64,
    pub source_unknown: f64,
    /// TrueHD, Atmos and DTS-HD MA.
    pub audio_top: f64,
    /// FLAC and LPCM.
    pub audio_lossless: f64,
    pub audio_eac3: f64,
    pub audio_dts: f64,
    pub audio_ac3: f64,
    pub audio_aac: f64,
    /// No audio tag, or one that is not ranked (Opus, MP3, Vorbis).
    pub audio_unknown: f64,
    /// HDR10 and HDR10+. Dolby Vision gets less than a Blu-ray remux's
    /// lead would suggest: Infuse's handling of profile 7 is unverified.
    pub hdr10: f64,
    pub dolby_vision: f64,
    /// Size earns `size_per_gib` per GiB up to `size_max_points`, and is
    /// never penalised for being large.
    pub size_per_gib: f64,
    pub size_max_points: f64,
    /// Seeders earn `seeder_factor * log10(1 + seeders)`, at most this many.
    pub seeder_factor: f64,
    pub seeder_max_points: f64,
    /// Added when a token of the name or file name is a preferred group.
    pub group_bonus: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RankSettings {
    // Hard filters.
    /// A torrent smaller than this is dropped.
    pub min_size_bytes: u64,
    /// A torrent larger than this is dropped from a free job's list.
    pub max_size_bytes: u64,
    /// The same for a PRO job, and the largest file a PRO job may be given:
    /// by the auto rule, and by the worker's own ceiling for a human's pick.
    /// Free jobs keep `max_size_bytes`, `auto_max_bytes` and `WORKER_RD_MAX_GB`.
    pub pro_max_bytes: u64,
    /// Languages that count as the wanted audio (ISO 639-1). A release whose
    /// language signals are all outside this list is penalised, not dropped.
    pub allowed_languages: Vec<String>,
    /// Names Real-Debrid refuses, matched against a candidate's torrent
    /// title and its file name. Env `WORKER_RD_BLOCKED_TERMS`; empty
    /// switches the filter off. The default is a third party's list as of
    /// 14 October 2026 and may be out of date.
    pub rd_blocked_terms: Vec<BlockedTerm>,

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

    // The PRO ranking.
    pub pro: ProPoints,
    /// Release groups (lowercase) that earn `pro.group_bonus` on a PRO job.
    /// Env `WORKER_PRO_GROUPS`, comma separated.
    pub pro_groups: Vec<String>,

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
    /// A PRO job's candidate needs at least this many seeders instead of
    /// `auto_min_seeders`, and has no minimum score. Code-only.
    pub pro_auto_min_seeders: u32,
    /// How many candidates an auto job tries at most.
    pub auto_max_tries: usize,
    /// A free job's candidate must have a known size of at most this. A PRO
    /// job's ceiling is `pro_max_bytes`.
    pub auto_max_bytes: u64,

    // What counts as "already had" for a PRO auto job: a downloaded release
    // in the library of at least this height and size. A free job's test is
    // the free profile, with no numbers. Env `WORKER_PRO_HAVE_MIN_HEIGHT`
    // and `WORKER_PRO_HAVE_MIN_GB`.
    pub pro_have_min_lines: u32,
    pub pro_have_min_bytes: u64,
}

impl Default for RankSettings {
    fn default() -> Self {
        Self {
            min_size_bytes: 400 * MIB,
            max_size_bytes: 40 * GIB,
            pro_max_bytes: 100 * GIB,
            allowed_languages: vec!["en".to_string()],
            rd_blocked_terms: rd_terms::default_terms(),
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
            pro: ProPoints {
                r2160: 45.0,
                r1440: 20.0,
                r1080: 30.0,
                r720: 10.0,
                sd: 0.0,
                resolution_unknown: 0.0,
                remux: 40.0,
                bluray: 28.0,
                web_dl: 22.0,
                web_rip: 14.0,
                hdtv: 8.0,
                hdrip: 6.0,
                dvd: 4.0,
                source_unknown: 4.0,
                audio_top: 20.0,
                audio_lossless: 18.0,
                audio_eac3: 10.0,
                audio_dts: 9.0,
                audio_ac3: 7.0,
                audio_aac: 4.0,
                audio_unknown: 6.0,
                hdr10: 6.0,
                dolby_vision: 8.0,
                size_per_gib: 0.4,
                size_max_points: 20.0,
                seeder_factor: 3.0,
                seeder_max_points: 8.0,
                group_bonus: 10.0,
            },
            pro_groups: vec!["framestor".to_string()],
            shortlist_len: 10,
            clear_min_score: 70.0,
            clear_margin: 8.0,
            dup_size_tolerance: 0.01,
            auto_min_score: 85.0,
            auto_min_seeders: 10,
            pro_auto_min_seeders: 3,
            auto_max_tries: 3,
            auto_max_bytes: 15 * GIB,
            pro_have_min_lines: 1080,
            pro_have_min_bytes: 8 * GIB,
        }
    }
}
