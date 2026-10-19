//! The hard filters and the free-Infuse rule.
//!
//! A candidate is dropped, for exactly one reason, when it cannot be the
//! right file at all (a pack, a cinema copy, another film). Everything else
//! stays and is judged by score. Language is deliberately not a filter: the
//! claim carries no original language, so a filter would drop every foreign
//! original. [`language_fit`] only says whether a release is a likely dub.

use std::collections::HashSet;

use super::{
    candidate::RawCandidate,
    release::{Audio, ParsedRelease, Video, parse_release, tokenize, years_in},
    settings::RankSettings,
};

/// What the worker knows about the job.
#[derive(Debug, Clone)]
pub struct JobSpec {
    /// The English TMDB title.
    pub title: String,
    pub year: Option<i32>,
    pub infuse_pro: bool,
    /// Infohashes marked "didn't play" for this request, lowercase.
    pub blocked: HashSet<String>,
}

/// Why a candidate was dropped. The variants are in the order the checks
/// run; the first that applies is the reason counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DropReason {
    Blocked,
    Pack,
    Extras,
    Disc,
    CamOrScreener,
    TitleMismatch,
    YearMismatch,
    Size,
    FreeProfile,
}

impl DropReason {
    pub fn as_str(self) -> &'static str {
        match self {
            DropReason::Blocked => "blocked",
            DropReason::Pack => "pack",
            DropReason::Extras => "extras",
            DropReason::Disc => "disc",
            DropReason::CamOrScreener => "cam_or_screener",
            DropReason::TitleMismatch => "title_mismatch",
            DropReason::YearMismatch => "year_mismatch",
            DropReason::Size => "size",
            DropReason::FreeProfile => "free_profile",
        }
    }
}

/// Will this play in the free version of Infuse? Only a claim read from the
/// release name; the shortlist says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FreeBadge {
    Yes,
    No,
    Unknown,
}

impl FreeBadge {
    pub fn as_str(self) -> &'static str {
        match self {
            FreeBadge::Yes => "yes",
            FreeBadge::No => "no",
            FreeBadge::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageFit {
    Ok,
    /// Every language signal is outside the wanted list: probably a dub.
    Foreign,
}

/// A candidate that passed the filters.
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    /// The release name, the file name and the source's label read together.
    pub release: ParsedRelease,
    pub badge: FreeBadge,
    pub language: LanguageFit,
}

/// `release` is the name, file name and label merged, so an unsafe tag in
/// any of them counts. `tags_may_be_cut` says the name was truncated and no
/// complete file name makes up for it: a cut-off tag could be anything, so a
/// would-be `Yes` is only `Unknown`.
///
/// `Yes` needs an explicit AAC tag and nothing else in the audio, no Dolby
/// Vision, and an H.264 or H.265 video tag. `No` is any other audio codec,
/// Dolby Vision, or another video codec. The rest is `Unknown`.
pub fn free_badge(release: &ParsedRelease, tags_may_be_cut: bool) -> FreeBadge {
    let unsafe_audio = release.audio.iter().any(|audio| *audio != Audio::Aac);
    let unsafe_video = matches!(
        release.video,
        Video::Av1 | Video::Xvid | Video::Divx | Video::Mpeg2 | Video::Vc1
    );
    if unsafe_audio || release.dolby_vision || unsafe_video {
        return FreeBadge::No;
    }
    let safe_video = matches!(release.video, Video::H264 | Video::H265);
    if !release.audio.is_empty() && safe_video && !tags_may_be_cut {
        FreeBadge::Yes
    } else {
        FreeBadge::Unknown
    }
}

pub fn passes_hard_filters(
    raw: &RawCandidate,
    job: &JobSpec,
    settings: &RankSettings,
) -> Result<Accepted, DropReason> {
    if job.blocked.contains(&raw.infohash) {
        return Err(DropReason::Blocked);
    }

    let name = parse_release(&raw.name);
    let file = raw.filename.as_deref().map(parse_release);
    let mut release = name.clone();
    if let Some(file) = &file {
        release.merge(file);
    }
    if let Some(label) = &raw.label {
        release.merge(&parse_release(label));
    }
    let title_tokens = tokenize(&job.title);

    let name_tokens = tokenize(&raw.name);
    let file_tokens = raw.filename.as_deref().map(tokenize);
    let name_run = title_run(&name_tokens, &title_tokens);
    let file_run = file_tokens
        .as_ref()
        .and_then(|tokens| title_run(tokens, &title_tokens));

    // A pack word that is part of the film's own title ("The Collection")
    // says nothing about the torrent.
    let outside_title = |words: &[&'static str]| {
        words
            .iter()
            .any(|word| !title_tokens.iter().any(|token| token == word))
    };
    // A torrent with several files is a collection of other titles unless
    // its own name is the film's: "Film (1994) RM4K (1080p ...)" holding the
    // film and some extras is a fine candidate, "Great Films 3" is not.
    let collection = raw.multi_file && name_run.is_none();
    if collection || outside_title(&release.flags.pack_words) {
        return Err(DropReason::Pack);
    }
    if outside_title(&release.flags.extras_words) {
        return Err(DropReason::Extras);
    }
    if release.flags.disc {
        return Err(DropReason::Disc);
    }
    if release.source.is_poor_copy() {
        return Err(DropReason::CamOrScreener);
    }

    if !title_tokens.is_empty() && name_run.is_none() && file_run.is_none() {
        return Err(DropReason::TitleMismatch);
    }

    if let Some(year) = job.year {
        // Years outside the matched title, so "1917" and "Blade Runner
        // 2049" do not read as release years.
        let mut years = years_in(&without_run(&name_tokens, name_run));
        if let Some(tokens) = &file_tokens {
            for y in years_in(&without_run(tokens, file_run)) {
                if !years.contains(&y) {
                    years.push(y);
                }
            }
        }
        if !years.is_empty() && !years.iter().any(|y| i32::from(*y) == year) {
            return Err(DropReason::YearMismatch);
        }
    }

    if let Some(size) = raw.size_bytes
        && (size < settings.min_size_bytes || size > settings.max_size_bytes)
    {
        return Err(DropReason::Size);
    }

    let file_is_complete = file.as_ref().is_some_and(|f| !f.truncated);
    let tags_may_be_cut = name.truncated && !file_is_complete;
    let badge = free_badge(&release, tags_may_be_cut);
    if !job.infuse_pro && badge == FreeBadge::No {
        return Err(DropReason::FreeProfile);
    }

    let language = language_fit(&release, raw, &settings.allowed_languages);
    Ok(Accepted {
        release,
        badge,
        language,
    })
}

/// Where the title starts in the release's tokens, as a token run. A
/// leading "the" is optional on the release's side, so "The Example Movie"
/// is also found as "Example Movie" (when that is at least two tokens).
fn title_run(haystack: &[String], title: &[String]) -> Option<(usize, usize)> {
    if title.is_empty() {
        return None;
    }
    if let Some(start) = find_run(haystack, title) {
        return Some((start, title.len()));
    }
    if title.len() >= 3 && title[0] == "the" {
        let short = &title[1..];
        if let Some(start) = find_run(haystack, short) {
            return Some((start, short.len()));
        }
    }
    None
}

fn find_run(haystack: &[String], needle: &[String]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn without_run(tokens: &[String], run: Option<(usize, usize)>) -> Vec<String> {
    match run {
        Some((start, len)) => tokens[..start]
            .iter()
            .chain(&tokens[start + len..])
            .cloned()
            .collect(),
        None => tokens.to_vec(),
    }
}

/// Reads the language signals of a candidate: the source's flags, language
/// words in the names, and non-Latin scripts. `Foreign` only when there are
/// signals, none of them is wanted, and nothing says "multi". Never drops
/// anything.
pub fn language_fit(
    release: &ParsedRelease,
    raw: &RawCandidate,
    allowed: &[String],
) -> LanguageFit {
    if raw.multi_audio || release.languages.contains(&super::release::Lang::Multi) {
        return LanguageFit::Ok;
    }
    let mut signals: Vec<&str> = raw.flag_languages.iter().map(String::as_str).collect();
    signals.extend(release.languages.iter().map(|lang| lang.code()));
    if release.scripts.cyrillic {
        signals.push("ru");
    }
    if release.scripts.cjk || release.scripts.other {
        signals.push("other");
    }
    if signals.is_empty() || signals.iter().any(|s| allowed.iter().any(|a| a == s)) {
        LanguageFit::Ok
    } else {
        LanguageFit::Foreign
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every release here is invented.

    fn raw(name: &str) -> RawCandidate {
        RawCandidate {
            infohash: "1".repeat(40),
            name: name.to_string(),
            filename: None,
            multi_file: false,
            label: None,
            size_bytes: Some(3 * 1024 * 1024 * 1024),
            seeders: Some(50),
            provider: Some("ExampleSite".to_string()),
            flag_languages: Vec::new(),
            multi_audio: false,
        }
    }

    fn job(title: &str, year: Option<i32>, pro: bool) -> JobSpec {
        JobSpec {
            title: title.to_string(),
            year,
            infuse_pro: pro,
            blocked: HashSet::new(),
        }
    }

    fn judge(raw: &RawCandidate, job: &JobSpec) -> Result<Accepted, DropReason> {
        passes_hard_filters(raw, job, &RankSettings::default())
    }

    const GOOD: &str = "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE";

    #[test]
    fn a_good_release_for_the_right_film_passes() {
        let accepted = judge(&raw(GOOD), &job("Example Movie", Some(1994), false)).unwrap();
        assert_eq!(accepted.badge, FreeBadge::Yes);
        assert_eq!(accepted.language, LanguageFit::Ok);
    }

    #[test]
    fn a_blocked_hash_is_dropped_first() {
        let mut spec = job("Example Movie", Some(1994), true);
        spec.blocked.insert("1".repeat(40));
        // Even a collection that would be dropped for another reason.
        let candidate = raw("Example Movie Collection 1994 1080p BluRay x264");
        assert_eq!(judge(&candidate, &spec), Err(DropReason::Blocked));
        spec.blocked.clear();
        assert_eq!(judge(&candidate, &spec), Err(DropReason::Pack));
    }

    #[test]
    fn packs_collections_and_extras_are_dropped() {
        let spec = job("Example Movie", Some(1994), true);
        assert_eq!(
            judge(
                &raw("Example Movie Collection 1994 1080p BluRay x264"),
                &spec
            ),
            Err(DropReason::Pack)
        );
        assert_eq!(
            judge(&raw("Example.Movie.1994.Featurettes.1080p.BluRay"), &spec),
            Err(DropReason::Extras)
        );
        assert_eq!(
            judge(&raw("Example.Movie.1994.1080p.sample"), &spec),
            Err(DropReason::Extras)
        );
        // The file inside a pack is looked at too.
        let mut inner = raw(GOOD);
        inner.filename = Some("Example Movie 1994 Trilogy 1080p.mkv".to_string());
        assert_eq!(judge(&inner, &spec), Err(DropReason::Pack));
    }

    #[test]
    fn a_multi_file_torrent_is_a_collection_unless_its_name_is_the_film() {
        let spec = job("Example Movie", Some(1994), true);
        // The torrent's own name has no film in it: a collection.
        let mut collection = raw("Great Examples 3: MP4 X264 AC3 1080p");
        collection.multi_file = true;
        collection.filename = Some("Example Movie (1994) 1080p Surround.mp4".to_string());
        assert_eq!(judge(&collection, &spec), Err(DropReason::Pack));
        // The torrent is named for the film and holds more files: fine.
        let mut film =
            raw("Example Movie (1994) RM4K (1080p BluRay x265 HEVC 10bit AAC 5.1 GrpTwo)");
        film.multi_file = true;
        film.filename =
            Some("Example Movie (1994) RM4K (1080p BluRay x265 GrpTwo).mkv".to_string());
        let accepted = judge(&film, &spec).unwrap();
        assert_eq!(accepted.badge, FreeBadge::Yes);
        // A single file named for something else is a title mismatch, not a pack.
        assert_eq!(
            judge(&raw("Great Examples 3: MP4 X264 AC3 1080p"), &spec),
            Err(DropReason::TitleMismatch)
        );
        // With no title to compare, a multi-file torrent cannot be vouched for.
        let untitled = job("", Some(1994), true);
        assert_eq!(judge(&film, &untitled), Err(DropReason::Pack));
    }

    #[test]
    fn a_pack_word_in_the_films_own_title_is_not_a_pack() {
        let spec = job("The Collection", Some(2012), true);
        assert!(judge(&raw("The.Collection.2012.1080p.BluRay.x264-GRPONE"), &spec).is_ok());
        // But a pack of other things still is.
        assert_eq!(
            judge(
                &raw("The.Collection.2012.Trilogy.1080p.BluRay.x264-GRPONE"),
                &spec
            ),
            Err(DropReason::Pack)
        );
    }

    #[test]
    fn disc_images_are_dropped() {
        let spec = job("Example Movie", Some(1994), true);
        for name in [
            "Example Movie [1994 Blu-ray disc 1080p]",
            "Example.Movie.1994.COMPLETE.BLURAY-GRPONE",
            "Example.Movie.1994.1080p.BDMV",
        ] {
            assert_eq!(judge(&raw(name), &spec), Err(DropReason::Disc), "{name}");
        }
    }

    #[test]
    fn cinema_copies_and_screeners_are_dropped_wherever_they_are_named() {
        let spec = job("Example Movie", Some(1994), true);
        assert_eq!(
            judge(&raw("Example.Movie.1994.HDCAM.x264-GRPONE"), &spec),
            Err(DropReason::CamOrScreener)
        );
        assert_eq!(
            judge(&raw("Example.Movie.1994.DVDSCR.XviD-GRPONE"), &spec),
            Err(DropReason::CamOrScreener)
        );
        // Only the file name says so.
        let mut file_only = raw("Example.Movie.1994.1080p.x264-GRPONE");
        file_only.filename = Some("Example.Movie.1994.HDTS.x264.mkv".to_string());
        assert_eq!(judge(&file_only, &spec), Err(DropReason::CamOrScreener));
        // A BluRay release with a stray "TS" is fine.
        assert!(judge(&raw("Example.Movie.1994.1080p.BluRay.TS.x264"), &spec).is_ok());
    }

    #[test]
    fn the_title_must_appear_as_a_run_of_tokens() {
        let pass =
            |title: &str, year: Option<i32>, name: &str| judge(&raw(name), &job(title, year, true));
        assert!(
            pass(
                "Spider-Man: No Way Home",
                Some(2021),
                "Spider.Man.No.Way.Home.2021.1080p.WEB-DL"
            )
            .is_ok()
        );
        assert!(pass("Se7en", Some(1995), "Se7en.1995.1080p.BluRay.x264").is_ok());
        assert!(pass("WALL-E", Some(2008), "WALL.E.2008.1080p.BluRay.x264").is_ok());
        assert!(pass("Amélie", Some(2001), "Amelie.2001.1080p.BluRay.x264").is_ok());
        assert!(pass("Fast & Furious", None, "Fast.and.Furious.2009.1080p").is_ok());
        assert!(pass("Schindler's List", Some(1993), "Schindlers.List.1993.1080p").is_ok());
        assert!(
            pass(
                "2001: A Space Odyssey",
                Some(1968),
                "2001.A.Space.Odyssey.1968.1080p.BluRay.x264"
            )
            .is_ok()
        );
        assert!(
            pass(
                "Blade Runner 2049",
                Some(2017),
                "Blade.Runner.2049.2017.1080p.BluRay.x264"
            )
            .is_ok()
        );
        assert!(pass("1917", Some(2019), "1917.2019.1080p.BluRay.x264").is_ok());
        // A leading "The" is optional on the release's side.
        assert!(pass("The Example Movie", Some(1994), GOOD).is_ok());
        // But a one-word remainder is too weak to count.
        assert_eq!(
            pass("The Room", Some(2003), "Panic.Room.2002.1080p.BluRay.x264"),
            Err(DropReason::TitleMismatch)
        );
        assert_eq!(
            pass("Other Film", Some(1994), GOOD),
            Err(DropReason::TitleMismatch)
        );
        // Words not in a row are not the title.
        assert_eq!(
            pass(
                "Example Movie",
                Some(1994),
                "Example.Other.Movie.1994.1080p"
            ),
            Err(DropReason::TitleMismatch)
        );
    }

    #[test]
    fn a_cyrillic_alternative_title_or_the_file_name_can_carry_the_title() {
        let spec = job("The Example Movie", Some(1994), true);
        assert!(
            judge(
                &raw("Пример Фильма / The Example Movie [1994 BDRip 1080p]"),
                &spec
            )
            .is_ok()
        );
        let mut by_file = raw("x264.AAC.1080p-GRPONE");
        by_file.filename = Some("The Example Movie (1994).mkv".to_string());
        assert!(judge(&by_file, &spec).is_ok());
        // A localised name alone is dropped, by design.
        assert_eq!(
            judge(&raw("Le.Film.Exemple.1994.1080p.BluRay"), &spec),
            Err(DropReason::TitleMismatch)
        );
    }

    #[test]
    fn the_year_must_match_when_the_release_names_one() {
        let spec = job("Example Movie", Some(1994), true);
        assert!(judge(&raw(GOOD), &spec).is_ok());
        assert_eq!(
            judge(&raw("Example.Movie.1995.1080p.BluRay.x264"), &spec),
            Err(DropReason::YearMismatch)
        );
        // No year in the release, or none asked for: nothing to compare.
        assert!(judge(&raw("Example.Movie.1080p.BluRay.x264"), &spec).is_ok());
        let no_year = job("Example Movie", None, true);
        assert!(judge(&raw("Example.Movie.1995.1080p.BluRay.x264"), &no_year).is_ok());
        // One right year among several is enough.
        assert!(judge(&raw("Example.Movie.1994.Remastered.2012.1080p"), &spec).is_ok());
        // The year may be in the file name only.
        let mut by_file = raw("Example.Movie.1080p.BluRay.x264");
        by_file.filename = Some("Example.Movie.1996.mkv".to_string());
        assert_eq!(judge(&by_file, &spec), Err(DropReason::YearMismatch));
        // A release year after the title that happens to equal the title.
        let numeric = job("1917", Some(2019), true);
        assert_eq!(
            judge(&raw("1917.2020.1080p.BluRay.x264"), &numeric),
            Err(DropReason::YearMismatch)
        );
    }

    #[test]
    fn the_size_must_be_in_range_when_known() {
        let spec = job("Example Movie", Some(1994), true);
        let settings = RankSettings::default();
        let sized = |size: Option<u64>| {
            let mut candidate = raw(GOOD);
            candidate.size_bytes = size;
            passes_hard_filters(&candidate, &spec, &settings)
        };
        assert_eq!(
            sized(Some(settings.min_size_bytes - 1)),
            Err(DropReason::Size)
        );
        assert!(sized(Some(settings.min_size_bytes)).is_ok());
        assert!(sized(Some(settings.max_size_bytes)).is_ok());
        assert_eq!(
            sized(Some(settings.max_size_bytes + 1)),
            Err(DropReason::Size)
        );
        assert!(sized(None).is_ok());
    }

    #[test]
    fn the_free_profile_drops_what_free_infuse_will_not_play() {
        let dts = "Example.Movie.1994.1080p.BluRay.DTS.x264-GRPONE";
        let free = job("Example Movie", Some(1994), false);
        let pro = job("Example Movie", Some(1994), true);
        assert_eq!(judge(&raw(dts), &free), Err(DropReason::FreeProfile));
        // The same release is fine for PRO, with the badge still saying no.
        assert_eq!(judge(&raw(dts), &pro).unwrap().badge, FreeBadge::No);
        for bad in [
            "Example.Movie.1994.1080p.WEB-DL.DDP5.1.H264-GRPONE",
            "Example.Movie.1994.2160p.WEB-DL.DV.HDR.AAC.H265-GRPONE",
            "Example.Movie.1994.1080p.BluRay.AV1.AAC-GRPONE",
            "Example.Movie.1994.1080p.BluRay.AAC.AC3.x264-GRPONE",
        ] {
            assert_eq!(
                judge(&raw(bad), &free),
                Err(DropReason::FreeProfile),
                "{bad}"
            );
            assert!(judge(&raw(bad), &pro).is_ok(), "{bad}");
        }
        // Unknown stays in the shortlist for a free job.
        let untagged = judge(&raw("Example.Movie.1994.1080p.BrRip.x264-GRPTWO"), &free).unwrap();
        assert_eq!(untagged.badge, FreeBadge::Unknown);
    }

    #[test]
    fn the_free_badge_follows_the_tags() {
        let badge = |name: &str| free_badge(&parse_release(name), false);
        assert_eq!(badge("Example.1994.1080p.BluRay.x264.AAC"), FreeBadge::Yes);
        assert_eq!(
            badge("Example.1994.2160p.BluRay.x265.10bit.HDR.AAC5.1"),
            FreeBadge::Yes
        );
        assert_eq!(badge("Example.1994.1080p.BluRay.x264.AC3"), FreeBadge::No);
        assert_eq!(
            badge("Example.1994.1080p.BluRay.x264.AAC.Atmos"),
            FreeBadge::No
        );
        assert_eq!(
            badge("Example.1994.1080p.BluRay.x264.AAC.DV"),
            FreeBadge::No
        );
        assert_eq!(badge("Example.1994.1080p.BluRay.AV1.AAC"), FreeBadge::No);
        assert_eq!(badge("Example.1994.1080p.BluRay.XviD.AAC"), FreeBadge::No);
        assert_eq!(badge("Example.1994.1080p.BluRay.x264.Opus"), FreeBadge::No);
        assert_eq!(badge("Example.1994.1080p.BluRay.x264"), FreeBadge::Unknown);
        assert_eq!(badge("Example.1994.1080p.BluRay.AAC"), FreeBadge::Unknown);
        assert_eq!(
            badge("Example.1994.1080p.BluRay.6CH.x265.HEVC"),
            FreeBadge::Unknown
        );
    }

    #[test]
    fn a_cut_off_name_cannot_earn_a_yes_but_can_still_be_a_no() {
        let cut = parse_release("Example.Movie.1994.1080p.BluRay.x264.AAC.5.1.GRPO…");
        assert_eq!(free_badge(&cut, true), FreeBadge::Unknown);
        assert_eq!(free_badge(&cut, false), FreeBadge::Yes);
        let cut_bad = parse_release("Example.Movie.1994.1080p.BluRay.x264.DTS.GRPO…");
        assert_eq!(free_badge(&cut_bad, true), FreeBadge::No);

        let spec = job("Example Movie", Some(1994), false);
        // A complete file name makes up for the cut name.
        let mut with_file = raw("Example.Movie.1994.1080p.BluRay.x264.AAC.5.1.GRPO…");
        with_file.filename =
            Some("Example.Movie.1994.1080p.BluRay.x264.AAC.5.1.GRPONE.mkv".to_string());
        assert_eq!(judge(&with_file, &spec).unwrap().badge, FreeBadge::Yes);
        // Without one it is only unknown.
        let alone = raw("Example.Movie.1994.1080p.BluRay.x264.AAC.5.1.GRPO…");
        assert_eq!(judge(&alone, &spec).unwrap().badge, FreeBadge::Unknown);
        // And a file name that shows an unsafe tag turns it into a no.
        let mut bad_file = raw("Example.Movie.1994.1080p.BluRay.x264.AAC.5.1.GRPO…");
        bad_file.filename = Some("Example.Movie.1994.1080p.BluRay.x264.DTS.mkv".to_string());
        assert_eq!(judge(&bad_file, &spec), Err(DropReason::FreeProfile));
    }

    #[test]
    fn the_sources_label_adds_a_dolby_vision_the_name_lost() {
        let spec = job("Example Movie", Some(1994), false);
        let mut cut = raw("Example.Movie.1994.2160p.BluRay.x265.AAC.GRPO…");
        cut.label = Some("4k DV | HDR".to_string());
        assert_eq!(judge(&cut, &spec), Err(DropReason::FreeProfile));
        cut.label = Some("4k HDR".to_string());
        assert!(judge(&cut, &spec).is_ok());
    }

    #[test]
    fn language_is_a_fit_and_never_a_filter() {
        let allowed = vec!["en".to_string()];
        let fit = |name: &str, flags: &[&str], multi: bool| {
            let mut candidate = raw(name);
            candidate.flag_languages = flags.iter().map(|f| f.to_string()).collect();
            candidate.multi_audio = multi;
            language_fit(&parse_release(name), &candidate, &allowed)
        };
        // A Cyrillic title flagged Russian only is a likely dub.
        assert_eq!(
            fit(
                "Пример Фильма / Example Movie [1994 BDRip 1080p]",
                &["ru"],
                false
            ),
            LanguageFit::Foreign
        );
        // The same with English flagged too is fine.
        assert_eq!(
            fit(
                "Пример Фильма / Example Movie [1994 BDRip 1080p]",
                &["en", "ru"],
                false
            ),
            LanguageFit::Ok
        );
        // Cyrillic with no flags still reads as Russian.
        assert_eq!(
            fit("Пример Фильма Example Movie 1994 BDRip", &[], false),
            LanguageFit::Foreign
        );
        // No signals at all.
        assert_eq!(fit(GOOD, &[], false), LanguageFit::Ok);
        // MULTi and a multi-audio flag are fine.
        assert_eq!(
            fit("Example.Movie.1994.MULTi.1080p", &[], false),
            LanguageFit::Ok
        );
        assert_eq!(fit(GOOD, &["it"], true), LanguageFit::Ok);
        // ENG.SUBS is not English audio: Italian audio with English subtitles.
        assert_eq!(
            fit("Example.Movie.1994.1080p.ITA.ENG.SUBS", &[], false),
            LanguageFit::Foreign
        );
        assert_eq!(
            fit(
                "Example.Movie.1994.1080p.ITA.ENG.SUBS",
                &["en", "it"],
                false
            ),
            LanguageFit::Ok
        );
        // A name with Japanese characters and no English signal.
        assert_eq!(
            fit("例子电影 Example Movie 1994 1080p", &[], false),
            LanguageFit::Foreign
        );
        // A different list of wanted languages changes the answer.
        let norwegian = vec!["en".to_string(), "no".to_string()];
        let name = "Example.Film.2020.NORWEGIAN.1080p";
        assert_eq!(
            language_fit(&parse_release(name), &raw(name), &allowed),
            LanguageFit::Foreign
        );
        assert_eq!(
            language_fit(&parse_release(name), &raw(name), &norwegian),
            LanguageFit::Ok
        );
    }

    #[test]
    fn a_foreign_language_candidate_is_kept_with_its_penalty_flag() {
        let spec = job("Example Movie", Some(1994), true);
        let mut dubbed = raw("Пример Фильма / Example Movie [1994 BDRip 1080p]");
        dubbed.flag_languages = vec!["ru".to_string()];
        let accepted = judge(&dubbed, &spec).unwrap();
        assert_eq!(accepted.language, LanguageFit::Foreign);
    }
}
