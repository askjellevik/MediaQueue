//! The worker's pure logic against a trimmed, invented Torrentio response
//! and a hand-written corpus of release names. No network. The repository is
//! public, so nothing in `tests/fixtures` comes from a real response: the
//! hashes are made up, the release names invented, only the layout is real.
//!
//! The golden rankings are written with
//! `UPDATE_GOLDEN=1 cargo test --test worker_golden`; read the diff before
//! committing it, a blessed bug is worse than no test.

use std::{collections::HashSet, fs, path::PathBuf};

use mediaqueue::worker::{
    Candidate, Evaluation,
    candidate::RawCandidate,
    evaluate,
    release::{Audio, parse_release, tokenize},
    rules::{DropReason, FreeBadge, JobSpec, free_badge},
    settings::RankSettings,
    source::{CandidateSource, MediaKind},
    torrentio::TorrentioSource,
};
use serde_json::Value;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const FIXTURE: &str = include_str!("fixtures/torrentio_example.json");
const CORPUS: &str = include_str!("fixtures/release_names.tsv");

fn h(n: u32) -> String {
    format!("{n:040x}")
}

/// Reads a Torrentio response through the real source, served from a local
/// stub, so the golden files cover the validator as well as the parser.
fn candidates_from(json: &str) -> Vec<RawCandidate> {
    tokio::runtime::Runtime::new()
        .expect("a runtime")
        .block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/stream/movie/tt1234567.json"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_raw(json.as_bytes(), "application/json"),
                )
                .mount(&server)
                .await;
            TorrentioSource::new(&server.uri())
                .expect("a client")
                .candidates("tt1234567", MediaKind::Movie)
                .await
                .expect("a readable response")
        })
}

fn job(pro: bool) -> JobSpec {
    JobSpec {
        title: "Example Movie".to_string(),
        year: Some(1994),
        infuse_pro: pro,
        blocked: HashSet::new(),
    }
}

fn run(pro: bool) -> Evaluation {
    evaluate(
        &job(pro),
        &candidates_from(FIXTURE),
        &RankSettings::default(),
    )
}

fn hashes(eval: &Evaluation) -> Vec<String> {
    eval.shortlist
        .iter()
        .map(|c| c.raw.infohash.clone())
        .collect()
}

fn find(eval: &Evaluation, n: u32) -> Option<&Candidate> {
    eval.shortlist.iter().find(|c| c.raw.infohash == h(n))
}

// ---------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------

#[test]
fn the_fixture_reads_into_fourteen_candidates_with_every_layout() {
    let candidates = candidates_from(FIXTURE);
    assert_eq!(candidates.len(), 14);
    let at = |n: u32| candidates.iter().find(|c| c.infohash == h(n)).unwrap();

    assert_eq!(at(1).seeders, Some(120));
    assert_eq!(at(1).flag_languages, ["en"]);
    assert!(at(2).filename.is_none(), "hints without a filename");
    assert!(at(5).filename.is_none(), "no hints at all");
    assert!(at(6).multi_file, "four lines: a pack");
    assert_eq!(
        at(6).filename.as_deref(),
        Some("Example Movie (1994) 1080p BluRay x264 AAC.mkv")
    );
    assert!(at(4).name.ends_with('\u{2026}'), "a name cut short");
    assert!(at(4).filename.as_deref().unwrap().contains("DDP5.1"));
    assert_eq!(at(7).flag_languages, ["en", "ru"]);
    assert_eq!(at(8).flag_languages, ["ru"]);
    assert_eq!(at(8).label.as_deref(), Some("BDRip"));
    assert_eq!(at(9).seeders, Some(698));
    assert_eq!(at(10).label, None, "a bare Torrentio name has no label");
    assert!(at(11).size_bytes.unwrap() < 1024 * 1024 * 1024, "megabytes");
    assert!(at(12).multi_audio);
    assert_eq!(at(12).flag_languages, ["en", "fr", "es"]);
    assert!(at(13).multi_file && at(14).multi_file);
    assert!(!at(1).multi_file && !at(7).multi_file);
    assert_eq!(at(3).label.as_deref(), Some("4k DV | HDR"));
}

// ---------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------

#[test]
fn every_release_in_the_corpus_reads_as_written() {
    let mut checked = 0;
    for (number, line) in CORPUS.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(
            cols.len(),
            9,
            "line {} has {} columns",
            number + 1,
            cols.len()
        );
        let name = cols[0];
        let parsed = parse_release(name);

        assert_eq!(
            parsed.resolution.lines().to_string(),
            cols[1],
            "resolution of {name}"
        );
        assert_eq!(parsed.source.as_str(), cols[2], "source of {name}");
        assert_eq!(parsed.video.as_str(), cols[3], "video of {name}");
        let audio: Vec<&str> = parsed.audio.iter().map(|a| a.as_str()).collect();
        let audio = if audio.is_empty() {
            "-".to_string()
        } else {
            audio.join(",")
        };
        assert_eq!(audio, cols[4], "audio of {name}");
        let dv = if parsed.dolby_vision { "yes" } else { "no" };
        assert_eq!(dv, cols[5], "dolby vision of {name}");
        assert_eq!(parsed.hdr.as_str(), cols[6], "hdr of {name}");
        assert_eq!(
            free_badge(&parsed, false).as_str(),
            cols[7],
            "free badge of {name}"
        );
        checked += 1;
    }
    assert!(checked >= 60, "the corpus should stay broad, has {checked}");
}

// ---------------------------------------------------------------------
// Golden rankings
// ---------------------------------------------------------------------

fn render(label: &str, eval: &Evaluation) -> String {
    let mut out = format!("# {label}\n");
    out += &format!("considered\t{}\n", eval.considered);
    out += &format!("survivors\t{}\n", eval.survivors);
    out += &format!("top\t{}\n", eval.top_infohash.as_deref().unwrap_or("-"));
    match eval.margin {
        Some(margin) => out += &format!("margin\t{margin:.1}\n"),
        None => out += "margin\t-\n",
    }
    out += &format!("clear_winner\t{}\n", eval.clear_winner);
    out += &format!(
        "foreign_language\t{}\t{}\n",
        eval.foreign_language, eval.foreign_in_shortlist
    );
    for (reason, count) in &eval.dropped {
        out += &format!("dropped\t{}\t{count}\n", reason.as_str());
    }
    out += "# rank\tinfohash\tscore\tfree_badge\tforeign_language\tclear_winner\n";
    for c in &eval.shortlist {
        out += &format!(
            "{}\t{}\t{:.1}\t{}\t{}\t{}\n",
            c.rank,
            c.raw.infohash,
            c.score,
            c.badge.as_str(),
            c.foreign_language,
            c.clear_winner
        );
    }
    out
}

fn golden(name: &str, label: &str, eval: &Evaluation) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let current = render(label, eval);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        fs::write(&path, &current).expect("write the golden file");
        return;
    }
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{name} is missing ({e}); run with UPDATE_GOLDEN=1"));
    assert_eq!(
        current, expected,
        "{name} differs; if the change is right, rerun with UPDATE_GOLDEN=1 and read the diff"
    );
}

#[test]
fn the_free_ranking_matches_its_golden_file() {
    golden(
        "golden_free.tsv",
        "free job: Example Movie (1994)",
        &run(false),
    );
}

#[test]
fn the_pro_ranking_matches_its_golden_file() {
    golden(
        "golden_pro.tsv",
        "pro job: Example Movie (1994)",
        &run(true),
    );
}

// ---------------------------------------------------------------------
// Facts a person can state about the fixture
// ---------------------------------------------------------------------

#[test]
fn the_collections_and_the_cinema_copy_are_dropped_for_every_job() {
    for pro in [false, true] {
        let eval = run(pro);
        // The pack with a pack word, and the collection with none.
        assert_eq!(eval.dropped.get(&DropReason::Pack), Some(&2), "pro={pro}");
        assert_eq!(
            eval.dropped.get(&DropReason::CamOrScreener),
            Some(&1),
            "pro={pro}"
        );
        for n in [6, 10, 14] {
            assert!(find(&eval, n).is_none(), "entry {n}, pro={pro}");
        }
        assert_eq!(eval.considered, 14);
    }
}

#[test]
fn a_multi_file_torrent_named_for_the_film_is_kept() {
    for pro in [false, true] {
        let eval = run(pro);
        let film = find(&eval, 13).expect("kept");
        assert_eq!(film.badge, FreeBadge::Yes, "pro={pro}");
        // A free job adds the verified-badge bonus.
        let expected = if pro { 105.4 } else { 111.4 };
        assert_eq!(film.score, expected, "pro={pro}");
    }
}

#[test]
fn the_russian_only_release_is_kept_penalised_and_never_the_winner() {
    for pro in [false, true] {
        let eval = run(pro);
        let dubbed = find(&eval, 8).expect("kept in the shortlist");
        assert!(dubbed.foreign_language, "pro={pro}");
        assert!(!dubbed.clear_winner);
        assert!(dubbed.parts.language < 0.0);
        let english_rank = find(&eval, 1).unwrap().rank;
        assert!(dubbed.rank > english_rank, "ranked below the English ones");
        // The Cyrillic title that also carries the English flag is not penalised.
        assert!(!find(&eval, 7).unwrap().foreign_language);
    }
}

#[test]
fn the_popular_untagged_release_is_unknown_and_does_not_win_a_free_job() {
    let eval = run(false);
    let untagged = find(&eval, 9).expect("in the free shortlist");
    assert_eq!(untagged.badge, FreeBadge::Unknown);
    assert!(!untagged.clear_winner && !eval.clear_winner);
    // It still ranks third, behind two verified AAC releases that the
    // badge bonus lifts above it.
    assert_eq!(eval.top_infohash, Some(h(1)));
    assert_eq!(eval.shortlist[1].raw.infohash, h(13));
    assert_eq!(eval.shortlist[2].raw.infohash, h(9));
}

#[test]
fn a_free_shortlist_holds_only_what_free_infuse_is_trusted_with() {
    let eval = run(false);
    assert!(!eval.shortlist.is_empty());
    for c in &eval.shortlist {
        assert!(c.badge != FreeBadge::No, "{}", c.raw.name);
        assert!(
            c.release.audio.iter().all(|a| *a == Audio::Aac),
            "{}",
            c.raw.name
        );
        assert!(!c.release.dolby_vision, "{}", c.raw.name);
    }
    assert_eq!(eval.dropped.get(&DropReason::FreeProfile), Some(&5));
    assert_eq!(eval.survivors, 6);
    // The five it dropped are the DTS+DV, the cut name with DDP in its file
    // name, DD5.1, XviD+AC3, and AV1+Opus.
    for n in [3, 4, 5, 11, 12] {
        assert!(find(&eval, n).is_none(), "entry {n}");
    }
}

#[test]
fn a_pro_job_keeps_what_the_free_job_dropped_and_labels_it() {
    let eval = run(true);
    for n in [3, 4, 5, 12] {
        let c = find(&eval, n).unwrap_or_else(|| panic!("entry {n} in the pro shortlist"));
        assert_eq!(c.badge, FreeBadge::No, "entry {n}");
    }
    assert!(!eval.dropped.contains_key(&DropReason::FreeProfile));
    // Eleven survive, ten fit on the shortlist: the weakest (the XviD DVDRip,
    // entry 11) is cut, not dropped.
    assert_eq!(eval.survivors, 11);
    assert_eq!(eval.shortlist.len(), 10);
    assert!(find(&eval, 11).is_none());
}

#[test]
fn the_cut_name_gets_its_audio_from_the_file_name() {
    let eval = run(true);
    let cut = find(&eval, 4).unwrap();
    assert!(cut.release.truncated);
    assert!(cut.release.audio.contains(&Audio::Eac3));
}

#[test]
fn a_clear_winner_appears_once_the_close_runners_up_are_blocked() {
    // The untagged popular release (9) and the multi-file film (13) are
    // within a few points of the top; block them and the lead is wide.
    for pro in [false, true] {
        let mut spec = job(pro);
        spec.blocked.insert(h(9));
        spec.blocked.insert(h(13));
        let eval = evaluate(&spec, &candidates_from(FIXTURE), &RankSettings::default());
        assert!(!hashes(&eval).contains(&h(9)), "pro={pro}");
        assert!(!hashes(&eval).contains(&h(13)), "pro={pro}");
        assert_eq!(eval.dropped.get(&DropReason::Blocked), Some(&2));
        assert!(eval.clear_winner, "pro={pro}");
        assert_eq!(eval.top_infohash, Some(h(1)));
        assert!(eval.shortlist[0].clear_winner);
        assert!(eval.shortlist[1..].iter().all(|c| !c.clear_winner));
    }
}

#[test]
fn every_shortlist_entry_is_ready_for_the_portal() {
    for pro in [false, true] {
        for c in run(pro).shortlist {
            let hash = &c.raw.infohash;
            assert!(
                hash.len() == 40 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            );
            let map = c.to_report_map();
            assert!(map.len() <= 24);
            assert_eq!(map["infohash"], Value::String(hash.clone()));
            for value in map.values() {
                if let Value::String(s) = value {
                    assert!(s.chars().count() <= 500);
                }
                assert!(!matches!(value, Value::Object(_)));
            }
        }
    }
}

// ---------------------------------------------------------------------
// The real response, kept outside the repository
// ---------------------------------------------------------------------

/// Run by hand against a saved real response (nothing from it is committed):
///
/// ```text
/// TORRENTIO_REAL_TITLE="..." TORRENTIO_REAL_YEAR=... \
/// TORRENTIO_REAL_FIXTURE=/path/to/response.json \
///   cargo test --test worker_golden -- --ignored --nocapture
/// ```
///
/// It asserts only structure and prints the drop counts and the shortlists,
/// to be read by a person.
#[test]
#[ignore = "needs a real response outside the repository"]
fn a_real_response_is_handled_sensibly() {
    let path = std::env::var("TORRENTIO_REAL_FIXTURE").expect("TORRENTIO_REAL_FIXTURE");
    let title = std::env::var("TORRENTIO_REAL_TITLE").expect("TORRENTIO_REAL_TITLE");
    let year: i32 = std::env::var("TORRENTIO_REAL_YEAR")
        .expect("TORRENTIO_REAL_YEAR")
        .parse()
        .expect("a year");
    let json = fs::read_to_string(&path).expect("read the real response");
    let raw = candidates_from(&json);
    println!("{} candidates read", raw.len());
    assert!(!raw.is_empty());

    for pro in [false, true] {
        let spec = JobSpec {
            title: title.clone(),
            year: Some(year),
            infuse_pro: pro,
            blocked: HashSet::new(),
        };
        let eval = evaluate(&spec, &raw, &RankSettings::default());
        println!(
            "\n{} job: considered {}, survivors {}, clear_winner {}, margin {:?}, foreign {} ({} in shortlist)",
            if pro { "PRO" } else { "FREE" },
            eval.considered,
            eval.survivors,
            eval.clear_winner,
            eval.margin,
            eval.foreign_language,
            eval.foreign_in_shortlist,
        );
        for (reason, count) in &eval.dropped {
            println!("  dropped {:<16} {count}", reason.as_str());
        }
        for c in &eval.shortlist {
            let name: String = c.raw.name.chars().take(90).collect();
            println!(
                "  {:>2}  {:>6.1}  {:<7} {} seeders {:>4}  {}",
                c.rank,
                c.score,
                c.badge.as_str(),
                if c.foreign_language {
                    "FOREIGN"
                } else {
                    "       "
                },
                c.raw.seeders.unwrap_or(0),
                name,
            );
        }

        for c in &eval.shortlist {
            let hash = &c.raw.infohash;
            assert!(
                hash.len() == 40 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            );
            if c.raw.multi_file {
                let named = tokenize(&c.raw.name).join(" ");
                let wanted = tokenize(&title).join(" ");
                assert!(
                    named.contains(&wanted),
                    "a collection reached the shortlist: {}",
                    c.raw.name
                );
            }
            assert!(!c.release.source.is_poor_copy(), "{}", c.raw.name);
            if c.foreign_language {
                assert!(!c.clear_winner);
            }
            if !pro {
                assert!(c.badge != FreeBadge::No, "{}", c.raw.name);
                assert!(
                    c.release.audio.iter().all(|a| *a == Audio::Aac),
                    "a non-AAC tag in a free shortlist: {}",
                    c.raw.name
                );
            }
        }
        if let Some(top) = eval.shortlist.first() {
            assert_eq!(
                eval.top_infohash.as_deref(),
                Some(top.raw.infohash.as_str())
            );
        }
    }
}
