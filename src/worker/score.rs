//! Scores the candidates that passed the filters, ranks them and decides
//! whether the top one is a clear winner.

use std::collections::{BTreeMap, HashSet};

use serde_json::{Map, Value, json};

use super::{
    candidate::RawCandidate,
    release::{Audio, Hdr, ParsedRelease, Resolution, Source, Video, tokenize},
    rules::{Accepted, DropReason, FreeBadge, JobSpec, LanguageFit, passes_hard_filters},
    settings::{RankSettings, SweetSpot},
};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// The portal rejects a string over 500 characters instead of cutting it.
const LONGEST_STRING: usize = 300;
/// The portal rejects a number past 1e12.
const LARGEST_NUMBER: u64 = 999_999_999_999;
const MOST_LANGUAGES: usize = 8;

/// The points a candidate earned, by part. `language` is zero or negative;
/// `free_badge` is zero unless a free job's candidate has a `yes` badge.
/// `codec` is a free job's part; `audio`, `hdr` and `group` are a PRO job's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreParts {
    pub resolution: f64,
    pub source: f64,
    pub codec: f64,
    pub size: f64,
    pub seeders: f64,
    pub language: f64,
    pub free_badge: f64,
    pub audio: f64,
    pub hdr: f64,
    pub group: f64,
}

impl ScoreParts {
    fn total(self) -> f64 {
        self.resolution
            + self.source
            + self.codec
            + self.size
            + self.seeders
            + self.language
            + self.free_badge
            + self.audio
            + self.hdr
            + self.group
    }
}

/// A candidate that passed the filters, with its score.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub raw: RawCandidate,
    pub release: ParsedRelease,
    pub badge: FreeBadge,
    pub foreign_language: bool,
    pub parts: ScoreParts,
    /// The total, rounded to one decimal.
    pub score: f64,
    /// 1 for the best.
    pub rank: usize,
    /// Only ever true on rank 1.
    pub clear_winner: bool,
    /// How many later near-identical candidates were folded into this one.
    pub similar: usize,
    /// 0 when an auto job may not try it, else its place (1 first) in the
    /// order an auto job would try the shortlist.
    pub auto_rank: usize,
    /// Language codes seen for the candidate: the source's flags and the
    /// languages named in the release.
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    /// The best candidates first, at most `shortlist_len`.
    pub shortlist: Vec<Candidate>,
    /// The best candidate, clear winner or not.
    pub top_infohash: Option<String>,
    pub clear_winner: bool,
    /// What the source offered, duplicates included.
    pub considered: usize,
    pub dropped: BTreeMap<DropReason, usize>,
    /// Candidates that passed the filters, before near-duplicates fold.
    pub survivors: usize,
    /// Survivors folded into an earlier near-identical one.
    pub collapsed: usize,
    /// The hashes an auto job would try, in order (see `assign_auto`).
    pub auto: Vec<String>,
    /// The top score minus the runner-up's, when there is a runner-up.
    pub margin: Option<f64>,
    /// Survivors with the foreign-language penalty, and how many of them
    /// made the shortlist.
    pub foreign_language: usize,
    pub foreign_in_shortlist: usize,
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn resolution_points(resolution: Resolution, settings: &RankSettings) -> f64 {
    let points = &settings.resolution;
    match resolution {
        Resolution::R2160 => points.r2160,
        Resolution::R1440 => points.r1440,
        Resolution::R1080 => points.r1080,
        Resolution::R720 => points.r720,
        Resolution::Sd => points.sd,
        Resolution::Unknown => points.unknown,
    }
}

fn source_points(source: Source, settings: &RankSettings) -> f64 {
    let points = &settings.source;
    match source {
        Source::BluRay => points.bluray,
        Source::WebDl => points.web_dl,
        Source::Remux => points.remux,
        Source::WebRip => points.web_rip,
        Source::Hdtv => points.hdtv,
        Source::Hdrip => points.hdrip,
        Source::Dvd => points.dvd,
        // Cinema copies never get here: the filters drop them.
        Source::Unknown | Source::Cam | Source::Telesync | Source::Screener => points.unknown,
    }
}

fn codec_points(video: Video, settings: &RankSettings) -> f64 {
    let points = &settings.codec;
    match video {
        Video::H264 => points.h264,
        Video::H265 => points.h265,
        Video::Av1 => points.av1,
        _ => points.other,
    }
}

fn sweet_spot(resolution: Resolution, settings: &RankSettings) -> SweetSpot {
    match resolution {
        Resolution::R2160 => settings.sweet_2160,
        Resolution::R1440 => settings.sweet_1440,
        Resolution::R720 => settings.sweet_720,
        Resolution::Sd => settings.sweet_sd,
        // An unknown height is judged like the common one.
        Resolution::R1080 | Resolution::Unknown => settings.sweet_1080,
    }
}

fn size_points(size_bytes: Option<u64>, resolution: Resolution, settings: &RankSettings) -> f64 {
    let Some(size) = size_bytes else {
        return 0.0;
    };
    let spot = sweet_spot(resolution, settings);
    let gib = size as f64 / GIB;
    let max = settings.size_max_points;
    if gib < spot.low_gib {
        let floor = spot.low_gib / 2.0;
        if gib <= floor {
            0.0
        } else {
            max * (gib - floor) / floor
        }
    } else if gib > spot.high_gib {
        let ceiling = spot.high_gib * 2.0;
        if gib >= ceiling {
            0.0
        } else {
            max * (ceiling - gib) / spot.high_gib
        }
    } else {
        max
    }
}

fn seeder_points(seeders: Option<u32>, settings: &RankSettings) -> f64 {
    let seeders = f64::from(seeders.unwrap_or(0));
    (settings.seeder_factor * (1.0 + seeders).log10()).min(settings.seeder_max_points)
}

fn score(
    raw: &RawCandidate,
    accepted: &Accepted,
    infuse_pro: bool,
    settings: &RankSettings,
) -> ScoreParts {
    let release = &accepted.release;
    ScoreParts {
        resolution: resolution_points(release.resolution, settings),
        source: source_points(release.source, settings),
        codec: codec_points(release.video, settings),
        size: size_points(raw.size_bytes, release.resolution, settings),
        seeders: seeder_points(raw.seeders, settings),
        language: if accepted.language == LanguageFit::Foreign {
            -settings.foreign_language_penalty
        } else {
            0.0
        },
        free_badge: if !infuse_pro && accepted.badge == FreeBadge::Yes {
            settings.free_badge_bonus
        } else {
            0.0
        },
        audio: 0.0,
        hdr: 0.0,
        group: 0.0,
    }
}

fn pro_resolution_points(resolution: Resolution, settings: &RankSettings) -> f64 {
    let pro = &settings.pro;
    match resolution {
        Resolution::R2160 => pro.r2160,
        Resolution::R1440 => pro.r1440,
        Resolution::R1080 => pro.r1080,
        Resolution::R720 => pro.r720,
        Resolution::Sd => pro.sd,
        Resolution::Unknown => pro.resolution_unknown,
    }
}

fn pro_source_points(source: Source, settings: &RankSettings) -> f64 {
    let pro = &settings.pro;
    match source {
        Source::Remux => pro.remux,
        Source::BluRay => pro.bluray,
        Source::WebDl => pro.web_dl,
        Source::WebRip => pro.web_rip,
        Source::Hdtv => pro.hdtv,
        Source::Hdrip => pro.hdrip,
        Source::Dvd => pro.dvd,
        Source::Unknown | Source::Cam | Source::Telesync | Source::Screener => pro.source_unknown,
    }
}

/// The points of the best audio codec named in the release.
fn pro_audio_points(audio: &[Audio], settings: &RankSettings) -> f64 {
    let pro = &settings.pro;
    audio
        .iter()
        .map(|a| match a {
            Audio::TrueHd | Audio::Atmos | Audio::DtsHd => pro.audio_top,
            Audio::Flac | Audio::Lpcm => pro.audio_lossless,
            Audio::Eac3 => pro.audio_eac3,
            Audio::Dts => pro.audio_dts,
            Audio::Ac3 => pro.audio_ac3,
            Audio::Aac => pro.audio_aac,
            Audio::Opus | Audio::Mp3 | Audio::Vorbis => pro.audio_unknown,
        })
        .fold(None, |best: Option<f64>, p| {
            Some(best.map_or(p, |b| b.max(p)))
        })
        .unwrap_or(pro.audio_unknown)
}

/// HDR10 or HDR10+ and Dolby Vision are not added up: the better of the two.
fn pro_hdr_points(release: &ParsedRelease, settings: &RankSettings) -> f64 {
    let pro = &settings.pro;
    let hdr = match release.hdr {
        Hdr::Hdr | Hdr::Hdr10Plus => pro.hdr10,
        Hdr::None | Hdr::Hlg => 0.0,
    };
    if release.dolby_vision {
        hdr.max(pro.dolby_vision)
    } else {
        hdr
    }
}

fn pro_size_points(size_bytes: Option<u64>, settings: &RankSettings) -> f64 {
    let gib = size_bytes.map_or(0.0, |n| n as f64 / GIB);
    (settings.pro.size_per_gib * gib).min(settings.pro.size_max_points)
}

fn pro_seeder_points(seeders: Option<u32>, settings: &RankSettings) -> f64 {
    let seeders = f64::from(seeders.unwrap_or(0));
    (settings.pro.seeder_factor * (1.0 + seeders).log10()).min(settings.pro.seeder_max_points)
}

/// Whether a token of the name or the file name is a preferred group.
fn pro_group_points(raw: &RawCandidate, settings: &RankSettings) -> f64 {
    let named = tokenize(&raw.name);
    let filed = raw.filename.as_deref().map(tokenize).unwrap_or_default();
    let preferred = settings.pro_groups.iter().any(|group| {
        let group = group.to_ascii_lowercase();
        named.iter().chain(&filed).any(|token| *token == group)
    });
    if preferred {
        settings.pro.group_bonus
    } else {
        0.0
    }
}

/// The PRO ranking: the best release possible, not the smallest. The
/// resolution, the source, the best audio, HDR, a size bonus that never
/// punishes a big file, the seeders and a preferred release group; the
/// foreign-language penalty is the free job's. `codec` and `free_badge` stay
/// zero: a PRO job has no use for them.
fn score_pro(raw: &RawCandidate, accepted: &Accepted, settings: &RankSettings) -> ScoreParts {
    let release = &accepted.release;
    ScoreParts {
        resolution: pro_resolution_points(release.resolution, settings),
        source: pro_source_points(release.source, settings),
        codec: 0.0,
        size: pro_size_points(raw.size_bytes, settings),
        seeders: pro_seeder_points(raw.seeders, settings),
        language: if accepted.language == LanguageFit::Foreign {
            -settings.foreign_language_penalty
        } else {
            0.0
        },
        free_badge: 0.0,
        audio: pro_audio_points(&release.audio, settings),
        hdr: pro_hdr_points(release, settings),
        group: pro_group_points(raw, settings),
    }
}

fn language_codes(raw: &RawCandidate, release: &ParsedRelease) -> Vec<String> {
    let mut codes: Vec<String> = Vec::new();
    let named = release.languages.iter().map(|lang| lang.code().to_string());
    for code in raw.flag_languages.iter().cloned().chain(named) {
        if !codes.contains(&code) && codes.len() < MOST_LANGUAGES {
            codes.push(code);
        }
    }
    codes
}

/// True when `a` and `b` are the same release in all but the group: equal
/// resolution, source, video, audio set, hdr, dv, bit10 and language fit, and
/// both sizes known and within `tolerance` of the earlier one's.
fn is_twin(earlier: &Candidate, later: &Candidate, tolerance: f64) -> bool {
    let (Some(a), Some(b)) = (earlier.raw.size_bytes, later.raw.size_bytes) else {
        return false;
    };
    let (e, l) = (&earlier.release, &later.release);
    e.resolution == l.resolution
        && e.source == l.source
        && e.video == l.video
        && e.audio == l.audio
        && e.hdr == l.hdr
        && e.dolby_vision == l.dolby_vision
        && e.bit10 == l.bit10
        && earlier.foreign_language == later.foreign_language
        && (a as f64 - b as f64).abs() <= a as f64 * tolerance
}

/// Folds each candidate into the first earlier one it is a twin of. The
/// input is sorted best first, so the better one stays and counts the rest in
/// `similar`. A folded candidate is never compared again, so a chain of
/// twins each 1% apart does not drift away from the one that stayed.
fn collapse_twins(sorted: Vec<Candidate>, tolerance: f64) -> Vec<Candidate> {
    let mut kept: Vec<Candidate> = Vec::new();
    for candidate in sorted {
        match kept
            .iter_mut()
            .find(|earlier| is_twin(earlier, &candidate, tolerance))
        {
            Some(earlier) => earlier.similar += 1,
            None => kept.push(candidate),
        }
    }
    kept
}

/// Whether an auto job may try the candidate. A free job needs a score of at
/// least `auto_min_score`, at least `auto_min_seeders` seeders, no foreign-
/// language penalty, a name that is not cut short, a known size of at most
/// `auto_max_bytes` and a `yes` badge. A PRO job needs no score and no badge:
/// at least `pro_auto_min_seeders` seeders, not foreign, not cut short and a
/// known size of at most `pro_max_bytes`; the PRO ranking orders them.
fn auto_eligible(candidate: &Candidate, job: &JobSpec, settings: &RankSettings) -> bool {
    let (min_score, min_seeders, largest) = if job.infuse_pro {
        (0.0, settings.pro_auto_min_seeders, settings.pro_max_bytes)
    } else {
        (
            settings.auto_min_score,
            settings.auto_min_seeders,
            settings.auto_max_bytes,
        )
    };
    candidate.score >= min_score
        && candidate.raw.seeders.is_some_and(|n| n >= min_seeders)
        && !candidate.foreign_language
        && !candidate.release.truncated
        && candidate.raw.size_bytes.is_some_and(|n| n <= largest)
        && (job.infuse_pro || candidate.badge == FreeBadge::Yes)
}

/// Walks the collapsed, ranked shortlist and numbers the first
/// `auto_max_tries` eligible candidates 1, 2, ...; returns their hashes in
/// that order. There is no margin: each is acceptable on its own, so the
/// best eligible one need not be rank 1. Empty when none is eligible.
fn assign_auto(shortlist: &mut [Candidate], job: &JobSpec, settings: &RankSettings) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    for candidate in shortlist {
        if order.len() >= settings.auto_max_tries {
            break;
        }
        if auto_eligible(candidate, job, settings) {
            order.push(candidate.raw.infohash.clone());
            candidate.auto_rank = order.len();
        }
    }
    order
}

/// Filters, scores and ranks what a source offered for one job.
///
/// `top_infohash` is the best survivor whether or not it is a clear winner:
/// the dry-run metric compares it with what a human picks. A clear winner
/// needs a top score of at least `clear_min_score`, a lead of at least
/// `clear_margin` over the runner-up (a lone survivor needs only the
/// score), a free-Infuse badge of `yes` unless the job is PRO, and no
/// foreign-language penalty. Only the top candidate can be one. Near-
/// identical survivors fold into the better one first (see `collapse_twins`),
/// so the margin, the winner and the shortlist see the collapsed list while
/// `survivors` counts the list before. `auto` lists what an auto job would
/// try; the clear winner stays a metric.
pub fn evaluate(job: &JobSpec, raw: &[RawCandidate], settings: &RankSettings) -> Evaluation {
    let mut dropped: BTreeMap<DropReason, usize> = BTreeMap::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut survivors: Vec<Candidate> = Vec::new();

    for candidate in raw {
        // The same torrent twice is one torrent. Not counted as a drop.
        if !seen.insert(candidate.infohash.as_str()) {
            continue;
        }
        match passes_hard_filters(candidate, job, settings) {
            Err(reason) => *dropped.entry(reason).or_insert(0) += 1,
            Ok(accepted) => {
                let parts = if job.infuse_pro {
                    score_pro(candidate, &accepted, settings)
                } else {
                    score(candidate, &accepted, false, settings)
                };
                survivors.push(Candidate {
                    languages: language_codes(candidate, &accepted.release),
                    raw: candidate.clone(),
                    badge: accepted.badge,
                    foreign_language: accepted.language == LanguageFit::Foreign,
                    release: accepted.release,
                    parts,
                    score: round1(parts.total()),
                    rank: 0,
                    clear_winner: false,
                    similar: 0,
                    auto_rank: 0,
                });
            }
        }
    }

    // Best score first; then more seeders; then the hash, so equal
    // candidates always come out in the same order.
    survivors.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.raw.seeders.unwrap_or(0).cmp(&a.raw.seeders.unwrap_or(0)))
            .then_with(|| a.raw.infohash.cmp(&b.raw.infohash))
    });

    let survivor_count = survivors.len();
    let foreign_language = survivors.iter().filter(|c| c.foreign_language).count();
    let mut survivors = collapse_twins(survivors, settings.dup_size_tolerance);
    let collapsed = survivor_count - survivors.len();
    let margin = match survivors.as_slice() {
        [top, second, ..] => Some(round1(top.score - second.score)),
        _ => None,
    };
    let clear_winner = survivors.first().is_some_and(|top| {
        top.score >= settings.clear_min_score
            && margin.is_none_or(|m| m >= settings.clear_margin)
            && (job.infuse_pro || top.badge == FreeBadge::Yes)
            && !top.foreign_language
    });

    survivors.truncate(settings.shortlist_len);
    for (i, candidate) in survivors.iter_mut().enumerate() {
        candidate.rank = i + 1;
    }
    if let Some(top) = survivors.first_mut() {
        top.clear_winner = clear_winner;
    }
    let foreign_in_shortlist = survivors.iter().filter(|c| c.foreign_language).count();
    let auto = assign_auto(&mut survivors, job, settings);

    Evaluation {
        top_infohash: survivors.first().map(|c| c.raw.infohash.clone()),
        shortlist: survivors,
        clear_winner,
        considered: raw.len(),
        dropped,
        survivors: survivor_count,
        collapsed,
        auto,
        margin,
        foreign_language,
        foreign_in_shortlist,
    }
}

/// At most `max` characters, with a trailing `…` when cut.
fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max - 1).collect();
    cut.push('…');
    cut
}

impl Candidate {
    /// The flat object the worker reports for this candidate: bounded
    /// scalars and short lists of them, no nesting, strings cut to what the
    /// portal accepts. The keys are what the admin shortlist reads.
    pub fn to_report_map(&self) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("infohash".into(), json!(self.raw.infohash));
        map.insert("title".into(), json!(cap(&self.raw.name, LONGEST_STRING)));
        if let Some(filename) = &self.raw.filename {
            map.insert("filename".into(), json!(cap(filename, LONGEST_STRING)));
        }
        map.insert("score".into(), json!(self.score));
        let p = self.parts;
        map.insert(
            "score_parts".into(),
            json!([
                round1(p.resolution),
                round1(p.source),
                round1(p.codec),
                round1(p.size),
                round1(p.seeders),
                round1(p.language),
                round1(p.free_badge),
                round1(p.audio),
                round1(p.hdr),
                round1(p.group),
            ]),
        );
        map.insert("resolution".into(), json!(self.release.resolution.lines()));
        map.insert("source".into(), json!(self.release.source.as_str()));
        map.insert("video".into(), json!(self.release.video.as_str()));
        let audio: Vec<&str> = self.release.audio.iter().map(|a| a.as_str()).collect();
        map.insert("audio".into(), json!(audio));
        map.insert("hdr".into(), json!(self.release.hdr.as_str()));
        map.insert("dv".into(), json!(self.release.dolby_vision));
        map.insert("bit10".into(), json!(self.release.bit10));
        if let Some(size) = self.raw.size_bytes {
            map.insert("size_bytes".into(), json!(size.min(LARGEST_NUMBER)));
        }
        if let Some(seeders) = self.raw.seeders {
            map.insert("seeders".into(), json!(seeders));
        }
        if let Some(provider) = &self.raw.provider {
            map.insert("provider".into(), json!(cap(provider, LONGEST_STRING)));
        }
        map.insert("languages".into(), json!(self.languages));
        map.insert("free_badge".into(), json!(self.badge.as_str()));
        map.insert("foreign_language".into(), json!(self.foreign_language));
        map.insert("truncated".into(), json!(self.release.truncated));
        map.insert("clear_winner".into(), json!(self.clear_winner));
        map.insert("similar".into(), json!(self.similar));
        // 23 keys with the `survivors` the run adds, of the portal's 24
        // (`MAX_CANDIDATE_KEYS` in `routes/internal.rs`): one key is left; the
        // key after that needs the limit raised there.
        map.insert("auto_rank".into(), json!(self.auto_rank));
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every release here is invented.

    const KIB: u64 = 1024;
    const GIB_BYTES: u64 = 1024 * 1024 * KIB;

    fn raw(hash_digit: char, name: &str, size_gib: f64, seeders: u32) -> RawCandidate {
        RawCandidate {
            infohash: hash_digit.to_string().repeat(40),
            name: name.to_string(),
            filename: None,
            multi_file: false,
            label: None,
            size_bytes: Some((size_gib * GIB_BYTES as f64) as u64),
            seeders: Some(seeders),
            provider: Some("ExampleSite".to_string()),
            flag_languages: Vec::new(),
            multi_audio: false,
        }
    }

    fn job(pro: bool) -> JobSpec {
        JobSpec {
            title: "Example Movie".to_string(),
            year: Some(1994),
            infuse_pro: pro,
            blocked: HashSet::new(),
        }
    }

    const BEST: &str = "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE";
    const WEB_720: &str = "Example.Movie.1994.720p.WEB-DL.x264.AAC-GRPONE";
    const UHD: &str = "Example.Movie.1994.2160p.BluRay.x265.AAC-GRPONE";

    fn open_settings() -> RankSettings {
        RankSettings {
            clear_min_score: 0.0,
            clear_margin: 0.0,
            ..RankSettings::unfiltered()
        }
    }

    #[test]
    fn the_size_points_peak_in_the_sweet_spot_and_fall_off_linearly() {
        let settings = RankSettings::unfiltered();
        let points = |gib: f64| size_points(Some((gib * GIB) as u64), Resolution::R1080, &settings);
        // 1080p: 2 to 12 GiB earns the full 15.
        assert_eq!(points(1.0), 0.0);
        assert_eq!(points(1.5), 7.5);
        assert_eq!(points(2.0), 15.0);
        assert_eq!(points(12.0), 15.0);
        assert_eq!(points(18.0), 7.5);
        assert_eq!(points(24.0), 0.0);
        assert_eq!(points(100.0), 0.0);
        assert_eq!(size_points(None, Resolution::R1080, &settings), 0.0);
        // An unknown height is judged like 1080p.
        assert_eq!(
            size_points(Some(3 * GIB_BYTES), Resolution::Unknown, &settings),
            15.0
        );
        // 2160p wants a bigger file.
        assert_eq!(
            size_points(Some(3 * GIB_BYTES), Resolution::R2160, &settings),
            0.0
        );
    }

    #[test]
    fn the_seeder_points_grow_slowly_and_stop_at_the_cap() {
        let settings = RankSettings::unfiltered();
        assert_eq!(seeder_points(Some(0), &settings), 0.0);
        assert_eq!(seeder_points(None, &settings), 0.0);
        assert_eq!(round1(seeder_points(Some(1), &settings)), 2.1);
        assert_eq!(round1(seeder_points(Some(698), &settings)), 19.9);
        assert_eq!(seeder_points(Some(1_000_000), &settings), 20.0);
    }

    const REMUX: &str = "Example.Movie.1994.2160p.UHD.BluRay.REMUX.HDR.HEVC.TrueHD.7.1-GRPONE";
    const SMALL: &str = "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE";

    #[test]
    fn a_pro_job_ranks_a_big_lossless_remux_above_a_small_encode() {
        let candidates = [raw('a', SMALL, 2.8, 400), raw('b', REMUX, 30.0, 40)];
        let pro = evaluate(&job(true), &candidates, &RankSettings::unfiltered());
        assert_eq!(order(&pro), "ba");
        let free = evaluate(&job(false), &candidates, &RankSettings::unfiltered());
        assert_eq!(free.shortlist[0].raw.infohash, "a".repeat(40));
    }

    #[test]
    fn the_pro_parts_add_up_to_the_weights() {
        let eval = evaluate(
            &job(true),
            &[raw('b', REMUX, 30.0, 40)],
            &RankSettings::unfiltered(),
        );
        let p = eval.shortlist[0].parts;
        assert_eq!(p.resolution, 45.0);
        assert_eq!(p.source, 40.0);
        assert_eq!(p.audio, 20.0);
        assert_eq!(p.hdr, 6.0);
        assert_eq!(p.size, 12.0, "0.4 points per GiB");
        assert_eq!(round1(p.seeders), 4.8);
        assert_eq!([p.codec, p.free_badge, p.group, p.language], [0.0; 4]);
    }

    #[test]
    fn the_best_audio_counts_and_dolby_vision_does_not_stack_on_hdr() {
        let settings = RankSettings::unfiltered();
        assert_eq!(
            pro_audio_points(&[Audio::Aac, Audio::Atmos], &settings),
            20.0
        );
        assert_eq!(pro_audio_points(&[Audio::Dts, Audio::Ac3], &settings), 9.0);
        assert_eq!(pro_audio_points(&[], &settings), 6.0);
        let release = crate::worker::release::parse_release(
            "Example.Movie.1994.2160p.WEB-DL.DV.HDR10Plus.x265.DDP5.1-GRPONE",
        );
        assert!(release.dolby_vision && release.hdr == Hdr::Hdr10Plus);
        assert_eq!(pro_hdr_points(&release, &settings), 8.0);
    }

    #[test]
    fn a_preferred_group_in_the_name_or_the_file_name_earns_the_bonus() {
        let settings = RankSettings::unfiltered();
        let mut named = raw(
            'a',
            "Example.Movie.1994.1080p.BluRay.x264.AAC-FRAMESTOR",
            8.0,
            5,
        );
        assert_eq!(pro_group_points(&named, &settings), 10.0);
        named.name = SMALL.to_string();
        assert_eq!(pro_group_points(&named, &settings), 0.0);
        named.filename = Some("Example Movie 1994 1080p framestor.mkv".to_string());
        assert_eq!(pro_group_points(&named, &settings), 10.0);
        let other = RankSettings {
            pro_groups: vec!["grpone".to_string()],
            ..settings
        };
        named.filename = None;
        assert_eq!(pro_group_points(&named, &other), 10.0, "a configured list");
    }

    #[test]
    fn candidates_are_ranked_by_score() {
        let eval = evaluate(
            &job(false),
            &[
                raw('c', UHD, 3.0, 50),
                raw('a', BEST, 3.0, 50),
                raw('b', WEB_720, 3.0, 50),
            ],
            &RankSettings::unfiltered(),
        );
        let order: Vec<(char, f64)> = eval
            .shortlist
            .iter()
            .map(|c| (c.raw.infohash.chars().next().unwrap(), c.score))
            .collect();
        // 1080p BluRay 40+30+10+15+12.0+6 (badge), 720p WEB 22+28+10+15+12.0+6,
        // 2160p BluRay 32+30+10+0+12.0+6 (3 GiB is too small for 2160p).
        assert_eq!(order, [('a', 113.0), ('b', 93.0), ('c', 90.0)]);
        assert_eq!(
            eval.shortlist.iter().map(|c| c.rank).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(eval.top_infohash.as_deref(), Some("a".repeat(40).as_str()));
        assert_eq!(eval.margin, Some(20.0));
        assert_eq!(eval.survivors, 3);
        assert_eq!(eval.considered, 3);
    }

    #[test]
    fn equal_candidates_come_out_in_hash_order_and_a_repeat_is_one_torrent() {
        let eval = evaluate(
            &job(false),
            &[
                raw('f', BEST, 3.0, 50),
                raw('2', BEST, 4.0, 50),
                raw('f', BEST, 3.0, 50),
                raw('9', BEST, 5.0, 50),
            ],
            &RankSettings::unfiltered(),
        );
        let hashes: Vec<char> = eval
            .shortlist
            .iter()
            .map(|c| c.raw.infohash.chars().next().unwrap())
            .collect();
        assert_eq!(hashes, ['2', '9', 'f']);
        assert_eq!(eval.considered, 4);
        assert!(eval.dropped.is_empty(), "a repeat is not a drop");
    }

    #[test]
    fn more_seeders_break_a_tie_before_the_hash_does() {
        // Same rounded score, different seeders: 20.0-capped seeder points.
        let eval = evaluate(
            &job(true),
            &[
                raw('1', BEST, 3.0, 1_000_000),
                raw('2', BEST, 3.0, 2_000_000),
            ],
            &RankSettings::unfiltered(),
        );
        assert_eq!(eval.shortlist[0].raw.infohash, "2".repeat(40));
    }

    #[test]
    fn the_shortlist_is_cut_but_the_counts_are_not() {
        // Sizes 2 to 9.5 GiB all earn the full size points but are no twins.
        let many: Vec<RawCandidate> = "0123456789abcdef"
            .chars()
            .take(15)
            .enumerate()
            .map(|(i, d)| raw(d, BEST, 2.0 + i as f64 * 0.5, 10))
            .collect();
        let eval = evaluate(&job(true), &many, &RankSettings::unfiltered());
        assert_eq!(eval.shortlist.len(), 10);
        assert_eq!(eval.survivors, 15);
        assert_eq!(eval.considered, 15);
    }

    /// The first hash digit of each shortlist entry, in order.
    fn order(eval: &Evaluation) -> String {
        eval.shortlist
            .iter()
            .map(|c| c.raw.infohash.chars().next().unwrap())
            .collect()
    }

    #[test]
    fn twins_fold_into_the_better_one_and_the_lead_is_the_gap_to_a_real_alternative() {
        let eval = evaluate(
            &job(false),
            &[
                raw('b', BEST, 3.02, 40),
                raw('a', BEST, 3.0, 50),
                raw('c', WEB_720, 3.0, 50),
            ],
            &open_settings(),
        );
        assert_eq!(order(&eval), "ac", "the better-scored twin stays");
        assert_eq!(eval.shortlist[0].similar, 1);
        assert_eq!(eval.shortlist[1].similar, 0);
        assert_eq!(eval.survivors, 3, "counted before folding");
        assert_eq!(eval.collapsed, 1);
        assert_eq!(eval.margin, Some(20.0), "to the 720p release, not the twin");
        assert_eq!(eval.shortlist[0].rank, 1);
        assert_eq!(eval.shortlist[1].rank, 2);
    }

    #[test]
    fn a_twin_no_longer_blocks_a_clear_winner() {
        let twins = [raw('a', BEST, 3.0, 50), raw('b', BEST, 3.02, 40)];
        let settings = RankSettings::unfiltered();
        let eval = evaluate(&job(false), &twins, &settings);
        assert!(eval.margin.is_none(), "one release is left");
        assert!(eval.clear_winner);
        let apart = [raw('a', BEST, 3.0, 50), raw('b', BEST, 3.2, 40)];
        let eval = evaluate(&job(false), &apart, &settings);
        assert!(eval.margin.is_some_and(|m| m < settings.clear_margin));
        assert!(!eval.clear_winner);
    }

    #[test]
    fn different_audio_or_hdr_do_not_collapse() {
        let eval = evaluate(
            &job(true),
            &[
                raw('a', BEST, 3.0, 50),
                raw(
                    'b',
                    "Example.Movie.1994.1080p.BluRay.x264.DTS-GRPONE",
                    3.0,
                    50,
                ),
                raw(
                    'c',
                    "Example.Movie.1994.1080p.BluRay.HDR.x264.AAC-GRPONE",
                    3.0,
                    50,
                ),
            ],
            &open_settings(),
        );
        assert_eq!(eval.shortlist.len(), 3);
        assert_eq!(eval.collapsed, 0);
        assert!(eval.shortlist.iter().all(|c| c.similar == 0));
    }

    #[test]
    fn sizes_further_apart_than_one_percent_do_not_collapse() {
        let eval = evaluate(
            &job(true),
            &[raw('a', BEST, 3.0, 50), raw('b', BEST, 3.045, 50)],
            &open_settings(),
        );
        assert_eq!(eval.shortlist.len(), 2, "1.5% apart");
    }

    #[test]
    fn an_unknown_size_never_collapses() {
        let mut unknown = raw('b', BEST, 3.0, 50);
        unknown.size_bytes = None;
        let mut also = raw('c', BEST, 3.0, 50);
        also.size_bytes = None;
        let eval = evaluate(
            &job(true),
            &[raw('a', BEST, 3.0, 50), unknown, also],
            &open_settings(),
        );
        assert_eq!(eval.shortlist.len(), 3);
        assert_eq!(eval.collapsed, 0);
    }

    #[test]
    fn the_tolerance_is_measured_from_the_one_that_stays() {
        // 0.9% apart each, 1.8% from the first: the third is not a twin.
        let eval = evaluate(
            &job(true),
            &[
                raw('a', BEST, 3.0, 50),
                raw('b', BEST, 3.027, 40),
                raw('c', BEST, 3.054, 30),
            ],
            &open_settings(),
        );
        assert_eq!(order(&eval), "ac");
        assert_eq!(eval.shortlist[0].similar, 1);
    }

    #[test]
    fn blocked_twins_are_still_drops_and_never_folded() {
        let mut spec = job(true);
        spec.blocked.insert("b".repeat(40));
        let eval = evaluate(
            &spec,
            &[raw('a', BEST, 3.0, 50), raw('b', BEST, 3.0, 50)],
            &open_settings(),
        );
        assert_eq!(eval.dropped.get(&DropReason::Blocked), Some(&1));
        assert_eq!(eval.survivors, 1);
        assert_eq!(eval.collapsed, 0);
        assert_eq!(eval.shortlist[0].similar, 0);
    }

    /// Settings with the auto thresholds off, so a test turns one on.
    fn lax_auto() -> RankSettings {
        RankSettings {
            auto_min_score: 0.0,
            auto_min_seeders: 0,
            ..open_settings()
        }
    }

    fn auto_hashes(eval: &Evaluation) -> String {
        eval.auto
            .iter()
            .map(|h| h.chars().next().unwrap())
            .collect()
    }

    #[test]
    fn a_free_job_skips_an_unknown_badge_down_to_the_next_yes() {
        // Untagged audio: badge unknown, and it ranks first on seeders.
        let untagged = raw('a', "Example.Movie.1994.1080p.BrRip.x264-GRPTWO", 3.0, 698);
        let tagged = raw('b', BEST, 3.0, 50);
        let eval = evaluate(
            &job(false),
            &[untagged.clone(), tagged.clone()],
            &RankSettings::unfiltered(),
        );
        assert_eq!(order(&eval), "ab");
        assert_eq!(eval.shortlist[0].badge, FreeBadge::Unknown);
        assert_eq!(auto_hashes(&eval), "b");
        assert_eq!(eval.shortlist[0].auto_rank, 0);
        assert_eq!(eval.shortlist[1].auto_rank, 1);
    }

    #[test]
    fn a_big_or_unsized_free_candidate_is_skipped() {
        let at_limit = RankSettings::unfiltered().auto_max_bytes;
        let mut exact = raw('a', BEST, 3.0, 50);
        exact.size_bytes = Some(at_limit);
        let mut over = raw('b', BEST, 3.0, 50);
        over.size_bytes = Some(at_limit + 5 * GIB_BYTES);
        let mut unknown = raw('c', BEST, 3.0, 50);
        unknown.size_bytes = None;
        let eval = evaluate(&job(false), &[exact, over, unknown], &lax_auto());
        assert_eq!(eval.shortlist.len(), 3, "all are ranked");
        assert_eq!(auto_hashes(&eval), "a", "exactly 15 GiB is in");
    }

    #[test]
    fn a_pro_candidate_may_be_far_bigger_than_the_free_ceiling_but_not_unsized() {
        let settings = RankSettings::unfiltered();
        let mut big = raw('a', BEST, 3.0, 50);
        big.size_bytes = Some(settings.auto_max_bytes + 25 * GIB_BYTES);
        let mut exact = raw('b', BEST, 3.0, 50);
        exact.size_bytes = Some(settings.pro_max_bytes);
        let mut unknown = raw('c', BEST, 3.0, 50);
        unknown.size_bytes = None;
        let eval = evaluate(&job(true), &[big, exact, unknown], &lax_auto());
        assert_eq!(auto_hashes(&eval), "ba");
    }

    #[test]
    fn a_pro_job_tries_the_best_releases_in_pro_order_without_a_badge_or_a_minimum_score() {
        let untagged = "Example.Movie.1994.1080p.BrRip.x264-GRPTWO";
        let remux = raw('a', REMUX, 45.0, 4);
        let encode = raw('b', SMALL, 2.8, 400);
        let unbadged = raw('c', untagged, 3.0, 3);
        let weak = raw('d', "Example.Movie.1994.480p.DVDRip.x264-GRPONE", 1.0, 20);
        let eval = evaluate(
            &job(true),
            &[
                encode.clone(),
                unbadged.clone(),
                weak.clone(),
                remux.clone(),
            ],
            &RankSettings::unfiltered(),
        );
        assert_eq!(order(&eval), "abcd");
        assert_eq!(auto_hashes(&eval), "abc", "three tries; no badge, no score");
        assert!(eval.shortlist[3].score < RankSettings::unfiltered().auto_min_score);
        assert_eq!(eval.shortlist[3].auto_rank, 0);

        // The free job takes neither the badge-less release nor the DVDRip.
        let free = evaluate(
            &job(false),
            &[encode, unbadged, weak],
            &RankSettings::unfiltered(),
        );
        assert_eq!(auto_hashes(&free), "b");
    }

    #[test]
    fn a_pro_job_skips_few_seeders_foreign_truncated_unsized_and_oversized() {
        let settings = RankSettings::unfiltered();
        let ok = raw('a', SMALL, 2.8, 3);
        let few = raw('b', SMALL, 3.2, 2);
        let mut foreign = raw('c', SMALL, 3.6, 50);
        foreign.flag_languages = vec!["ru".to_string()];
        let cut = raw(
            'd',
            "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPO…",
            4.0,
            50,
        );
        let mut unsized_ = raw('e', SMALL, 2.8, 50);
        unsized_.size_bytes = None;
        let mut over = raw('f', SMALL, 2.8, 50);
        over.size_bytes = Some(settings.pro_max_bytes + 1);
        let all = [ok, few, foreign, cut, unsized_, over];
        let eval = evaluate(&job(true), &all, &settings);
        assert_eq!(eval.dropped.get(&DropReason::Size), Some(&1));
        assert_eq!(eval.shortlist.len(), 5);
        assert_eq!(auto_hashes(&eval), "a");
    }

    #[test]
    fn low_seeders_foreign_truncated_and_low_scores_are_skipped() {
        let mut foreign = raw('b', BEST, 3.0, 50);
        foreign.flag_languages = vec!["ru".to_string()];
        let cut = raw(
            'c',
            "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPO…",
            4.0,
            50,
        );
        let few = raw('d', BEST, 6.0, 9);
        let weak = raw('e', "Example.Movie.1994.720p.HDTV.x264.AAC-GRPONE", 0.5, 50);
        let fine = raw('a', BEST, 3.0, 10);
        let all = [foreign, cut, few, weak.clone(), fine];

        let eval = evaluate(&job(false), &all, &RankSettings::unfiltered());
        assert!(
            eval.shortlist
                .iter()
                .any(|c| c.raw.infohash.starts_with('c'))
        );
        assert_eq!(auto_hashes(&eval), "a", "10 seeders is enough, 9 is not");

        // Each rule alone, with the others off.
        let only = |settings: RankSettings| auto_hashes(&evaluate(&job(false), &all, &settings));
        let none = RankSettings {
            auto_min_score: 0.0,
            auto_min_seeders: 0,
            ..RankSettings::unfiltered()
        };
        assert_eq!(
            only(none.clone()).len(),
            3,
            "foreign and truncated stay out"
        );
        assert!(!only(none.clone()).contains(['b', 'c']));
        let score_only = RankSettings {
            auto_min_score: 80.0,
            ..none.clone()
        };
        assert!(!only(score_only).contains('e'), "{}", weak.name);
        let seeders_only = RankSettings {
            auto_min_seeders: 10,
            ..none
        };
        assert!(!only(seeders_only).contains('d'));
    }

    #[test]
    fn an_auto_job_tries_at_most_the_configured_number() {
        let many: Vec<RawCandidate> = "0123456"
            .chars()
            .enumerate()
            .map(|(i, d)| raw(d, BEST, 2.0 + i as f64, 50))
            .collect();
        let eval = evaluate(&job(true), &many, &lax_auto());
        assert_eq!(eval.auto.len(), 3, "the default is three");
        let ranks: Vec<usize> = eval.shortlist.iter().map(|c| c.auto_rank).collect();
        assert_eq!(ranks, [1, 2, 3, 0, 0, 0, 0]);
        let five = RankSettings {
            auto_max_tries: 5,
            ..lax_auto()
        };
        assert_eq!(evaluate(&job(true), &many, &five).auto.len(), 5);
    }

    #[test]
    fn with_nothing_eligible_the_auto_list_is_empty() {
        let eval = evaluate(
            &job(false),
            &[raw('a', BEST, 3.0, 2), raw('b', WEB_720, 3.0, 3)],
            &RankSettings::unfiltered(),
        );
        assert!(eval.auto.is_empty());
        assert!(eval.shortlist.iter().all(|c| c.auto_rank == 0));
        assert!(
            evaluate(&job(false), &[], &RankSettings::unfiltered())
                .auto
                .is_empty()
        );
    }

    #[test]
    fn the_auto_walk_follows_the_collapsed_list() {
        // The twin folds away and so is never a second auto try.
        let eval = evaluate(
            &job(false),
            &[
                raw('a', BEST, 3.0, 50),
                raw('b', BEST, 3.02, 40),
                raw('c', WEB_720, 3.0, 50),
            ],
            &RankSettings::unfiltered(),
        );
        assert_eq!(auto_hashes(&eval), "ac");
    }

    #[test]
    fn blocked_and_unfit_candidates_are_counted_and_never_ranked() {
        let mut spec = job(true);
        spec.blocked.insert("a".repeat(40));
        let eval = evaluate(
            &spec,
            &[
                raw('a', BEST, 3.0, 50),
                raw('b', "Example.Movie.1994.HDCAM.x264-GRPONE", 3.0, 50),
                raw('c', "Other.Film.1994.1080p.BluRay.x264", 3.0, 50),
                raw('d', WEB_720, 3.0, 50),
            ],
            &RankSettings::unfiltered(),
        );
        assert_eq!(eval.shortlist.len(), 1);
        assert_eq!(eval.shortlist[0].raw.infohash, "d".repeat(40));
        assert_eq!(eval.dropped[&DropReason::Blocked], 1);
        assert_eq!(eval.dropped[&DropReason::CamOrScreener], 1);
        assert_eq!(eval.dropped[&DropReason::TitleMismatch], 1);
    }

    #[test]
    fn names_real_debrid_refuses_are_counted_and_never_ranked_in_either_profile() {
        let input = [
            raw(
                'a',
                "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE",
                3.0,
                50,
            ),
            raw(
                'b',
                "Example.Movie.1994.1080p.WEB-DL.h264.AAC-GRPONE",
                3.0,
                50,
            ),
            raw(
                'c',
                "Example.Movie.1994.1080p.BluRay.h264.AAC-GRPONE",
                3.0,
                50,
            ),
        ];
        for pro in [false, true] {
            let eval = evaluate(&job(pro), &input, &RankSettings::default());
            assert_eq!(eval.dropped[&DropReason::RdBlocked], 2, "pro={pro}");
            assert_eq!(DropReason::RdBlocked.as_str(), "rd_blocked");
            assert_eq!(eval.survivors, 1, "pro={pro}");
            assert_eq!(eval.shortlist[0].raw.infohash, "c".repeat(40));
        }
    }

    #[test]
    fn free_and_pro_jobs_see_different_shortlists_of_the_same_input() {
        let input = [
            raw(
                'a',
                "Example.Movie.1994.1080p.BluRay.DTS.x264-GRPONE",
                3.0,
                50,
            ),
            raw('b', WEB_720, 3.0, 50),
        ];
        let free = evaluate(&job(false), &input, &RankSettings::unfiltered());
        let pro = evaluate(&job(true), &input, &RankSettings::unfiltered());
        assert_eq!(free.shortlist.len(), 1);
        assert_eq!(free.shortlist[0].raw.infohash, "b".repeat(40));
        assert_eq!(free.dropped[&DropReason::FreeProfile], 1);
        assert_eq!(pro.shortlist.len(), 2);
        assert_eq!(pro.shortlist[0].raw.infohash, "a".repeat(40));
        assert_eq!(pro.shortlist[0].badge, FreeBadge::No);
    }

    #[test]
    fn nothing_to_rank_means_no_top_and_no_winner() {
        let eval = evaluate(&job(true), &[], &RankSettings::unfiltered());
        assert!(eval.shortlist.is_empty() && eval.top_infohash.is_none());
        assert!(!eval.clear_winner && eval.margin.is_none());
        let all_bad = evaluate(
            &job(true),
            &[raw('a', "Example.Movie.1994.HDCAM.x264", 3.0, 5)],
            &RankSettings::unfiltered(),
        );
        assert!(all_bad.top_infohash.is_none() && !all_bad.clear_winner);
        assert_eq!(all_bad.considered, 1);
    }

    #[test]
    fn a_lone_survivor_needs_only_the_minimum_score() {
        let input = [raw('a', BEST, 3.0, 50)]; // scores 113.0
        let at = |min: f64| {
            evaluate(
                &job(false),
                &input,
                &RankSettings {
                    clear_min_score: min,
                    ..RankSettings::unfiltered()
                },
            )
        };
        assert!(at(113.0).clear_winner, "exactly the minimum is enough");
        assert!(!at(113.1).clear_winner);
        assert!(at(113.0).shortlist[0].clear_winner);
    }

    #[test]
    fn the_lead_over_the_runner_up_must_reach_the_margin() {
        let input = [raw('a', BEST, 3.0, 50), raw('b', WEB_720, 3.0, 50)];
        let margin = evaluate(&job(false), &input, &open_settings())
            .margin
            .unwrap();
        assert_eq!(margin, 20.0);
        let with_margin = |m: f64| {
            evaluate(
                &job(false),
                &input,
                &RankSettings {
                    clear_margin: m,
                    clear_min_score: 0.0,
                    ..RankSettings::unfiltered()
                },
            )
        };
        assert!(with_margin(margin).clear_winner, "equal to the margin");
        assert!(!with_margin(margin + 0.1).clear_winner);
    }

    #[test]
    fn only_the_top_candidate_carries_the_clear_winner_flag() {
        let eval = evaluate(
            &job(true),
            &[raw('a', BEST, 3.0, 50), raw('b', WEB_720, 3.0, 50)],
            &open_settings(),
        );
        assert!(eval.clear_winner);
        let flags: Vec<bool> = eval.shortlist.iter().map(|c| c.clear_winner).collect();
        assert_eq!(flags, [true, false]);
    }

    #[test]
    fn a_free_job_needs_a_yes_badge_to_have_a_clear_winner() {
        // Untagged audio: badge unknown. Scores high all the same.
        let untagged = raw('a', "Example.Movie.1994.1080p.BrRip.x264-GRPTWO", 3.0, 698);
        let free = evaluate(
            &job(false),
            std::slice::from_ref(&untagged),
            &open_settings(),
        );
        assert_eq!(free.shortlist[0].badge, FreeBadge::Unknown);
        assert!(!free.clear_winner && free.top_infohash.is_some());
        let pro = evaluate(
            &job(true),
            std::slice::from_ref(&untagged),
            &open_settings(),
        );
        assert!(pro.clear_winner, "the badge only matters for a free job");
        // With an AAC tag the same job can win.
        let tagged = evaluate(&job(false), &[raw('a', BEST, 3.0, 50)], &open_settings());
        assert!(tagged.clear_winner);
    }

    /// A verified (`yes`) WEB-DL and an unverified BluRay that leads it by
    /// `lead` points before the bonus. Seeders are off so only the source and
    /// the bonus differ.
    fn verified_against_unverified(pro: bool, lead: f64) -> Evaluation {
        let mut settings = open_settings();
        settings.seeder_factor = 0.0;
        settings.source.web_dl = 20.0;
        settings.source.bluray = 20.0 + lead;
        let verified = raw(
            'a',
            "Example.Movie.1994.1080p.WEB-DL.x264.AAC-GRPONE",
            3.0,
            50,
        );
        let unverified = raw('b', "Example.Movie.1994.1080p.BluRay.x264-GRPTWO", 3.0, 50);
        evaluate(&job(pro), &[verified, unverified], &settings)
    }

    #[test]
    fn a_verified_badge_earns_a_free_job_a_bonus_of_about_six_points() {
        let top = |lead: f64| {
            let eval = verified_against_unverified(false, lead);
            eval.shortlist[0].raw.infohash.chars().next().unwrap()
        };
        assert_eq!(top(5.0), 'a', "five points ahead is not enough");
        assert_eq!(top(7.0), 'b', "seven points ahead is");
        let eval = verified_against_unverified(false, 5.0);
        assert_eq!(eval.shortlist[0].parts.free_badge, 6.0);
        assert_eq!(eval.shortlist[1].parts.free_badge, 0.0);
    }

    #[test]
    fn a_pro_job_scores_without_the_badge_bonus() {
        let eval = verified_against_unverified(true, 5.0);
        assert!(eval.shortlist.iter().all(|c| c.parts.free_badge == 0.0));
        assert_eq!(eval.shortlist[0].raw.infohash.chars().next(), Some('b'));
        let map = eval.shortlist[0].to_report_map();
        assert_eq!(map["score_parts"].as_array().unwrap().len(), 10);
        assert_eq!(map["score_parts"][6], json!(0.0));
    }

    #[test]
    fn a_foreign_language_candidate_is_penalised_kept_and_never_a_clear_winner() {
        let mut dubbed = raw('b', BEST, 3.0, 50);
        dubbed.flag_languages = vec!["ru".to_string()];
        let english = raw('a', BEST, 3.0, 50);

        let eval = evaluate(&job(true), &[dubbed.clone(), english], &open_settings());
        assert_eq!(eval.shortlist.len(), 2, "the penalty never drops one");
        assert_eq!(eval.shortlist[0].raw.infohash, "a".repeat(40));
        assert!(eval.shortlist[1].foreign_language);
        assert_eq!(
            eval.shortlist[0].score - eval.shortlist[1].score,
            25.0,
            "the penalty is 25 points"
        );
        assert_eq!(eval.shortlist[1].parts.language, -25.0);
        assert_eq!((eval.foreign_language, eval.foreign_in_shortlist), (1, 1));

        // Alone, and scoring well, it still is not a clear winner.
        for pro in [true, false] {
            let alone = evaluate(&job(pro), &[dubbed.clone()], &open_settings());
            assert!(!alone.clear_winner, "pro={pro}");
            assert_eq!(alone.top_infohash.as_deref(), Some("b".repeat(40).as_str()));
        }
    }

    #[test]
    fn a_foreign_top_blocks_the_win_even_with_a_big_lead() {
        // Foreign: 500 seeders and a lot of points, still ahead of a weak one.
        let mut dubbed = raw('a', BEST, 3.0, 500);
        dubbed.flag_languages = vec!["ru".to_string()];
        let weak = raw('b', "Example.Movie.1994.480p.DVDRip.x264-GRPONE", 1.0, 1);
        let eval = evaluate(&job(true), &[dubbed, weak], &open_settings());
        assert_eq!(eval.shortlist[0].raw.infohash, "a".repeat(40));
        assert!(eval.margin.unwrap() > 8.0);
        assert!(!eval.clear_winner);
    }

    #[test]
    fn the_report_map_stays_inside_what_the_portal_accepts() {
        let long = format!(
            "Example.Movie.1994.1080p.BluRay.x264.AAC.{}",
            "x".repeat(600)
        );
        let mut big = raw('a', &long, 3.0, 50);
        big.filename = Some(format!("{long}.mkv"));
        big.provider = Some("p".repeat(400));
        big.size_bytes = Some(5_000_000_000_000);
        let mut many: Vec<RawCandidate> = "bcdefghij"
            .chars()
            .enumerate()
            .map(|(i, _)| {
                let mut c = raw('b', BEST, 2.0 + i as f64, 50);
                c.infohash = format!("{i:040x}");
                c
            })
            .collect();
        many.push(big);
        let settings = RankSettings {
            max_size_bytes: u64::MAX,
            pro_max_bytes: u64::MAX,
            ..RankSettings::unfiltered()
        };
        let eval = evaluate(&job(true), &many, &settings);
        assert_eq!(
            eval.shortlist.len(),
            10,
            "nine ordinary ones and the long one"
        );

        let maps: Vec<Map<String, Value>> = eval
            .shortlist
            .iter()
            .map(Candidate::to_report_map)
            .collect();
        for map in &maps {
            assert!(map.len() <= 24, "{} keys", map.len());
            for (key, value) in map {
                assert!(!key.is_empty() && key.chars().count() <= 64);
                let scalars: Vec<&Value> = match value {
                    Value::Array(items) => {
                        assert!(items.len() <= 32, "{key}");
                        items.iter().collect()
                    }
                    other => vec![other],
                };
                for scalar in scalars {
                    match scalar {
                        Value::String(s) => assert!(s.chars().count() <= 500, "{key}"),
                        Value::Number(n) => {
                            let f = n.as_f64().unwrap();
                            assert!(f.is_finite() && f.abs() <= 1e12, "{key}");
                        }
                        Value::Bool(_) | Value::Null => {}
                        other => panic!("{key} is nested: {other}"),
                    }
                }
            }
            let hash = map["infohash"].as_str().unwrap();
            assert!(
                hash.len() == 40
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            );
        }
        assert!(serde_json::to_string(&maps).unwrap().len() < 64 * 1024);

        // The long fields were cut with a mark, the size was clamped.
        let cut = maps
            .iter()
            .find(|m| m["title"].as_str().unwrap().ends_with('…'));
        let cut = cut.expect("the long name is in the shortlist");
        assert_eq!(cut["title"].as_str().unwrap().chars().count(), 300);
        assert_eq!(cut["size_bytes"], json!(999_999_999_999u64));
        assert_eq!(cut["provider"].as_str().unwrap().chars().count(), 300);
    }

    #[test]
    fn the_report_map_names_every_key_step_three_reads() {
        let eval = evaluate(&job(false), &[raw('a', BEST, 3.0, 50)], &open_settings());
        let map = eval.shortlist[0].to_report_map();
        let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = vec![
            "infohash",
            "title",
            "score",
            "score_parts",
            "resolution",
            "source",
            "video",
            "audio",
            "hdr",
            "dv",
            "bit10",
            "size_bytes",
            "seeders",
            "provider",
            "languages",
            "free_badge",
            "foreign_language",
            "truncated",
            "clear_winner",
            "similar",
            "auto_rank",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected, "no filename: the source gave none");
        assert!(!map.contains_key("rank"), "the position is the rank");
        assert_eq!(map["similar"], json!(0));
        assert_eq!(map["auto_rank"], json!(1));
        assert_eq!(map["score"], json!(113.0));
        assert_eq!(
            map["score_parts"],
            json!([40.0, 30.0, 10.0, 15.0, 12.0, 0.0, 6.0, 0.0, 0.0, 0.0])
        );
        assert_eq!(map["resolution"], json!(1080));
        assert_eq!(map["source"], json!("bluray"));
        assert_eq!(map["video"], json!("h264"));
        assert_eq!(map["audio"], json!(["aac"]));
        assert_eq!(map["free_badge"], json!("yes"));
        assert_eq!(map["clear_winner"], json!(true));
        assert_eq!(map["foreign_language"], json!(false));
    }

    #[test]
    fn a_known_file_name_adds_its_key() {
        let mut candidate = raw('a', BEST, 3.0, 50);
        candidate.filename = Some("Example.Movie.1994.mkv".to_string());
        let eval = evaluate(&job(true), &[candidate], &open_settings());
        let map = eval.shortlist[0].to_report_map();
        assert_eq!(map["filename"], json!("Example.Movie.1994.mkv"));
        assert_eq!(map.len(), 22, "23 with the run's survivors key");
    }
}
