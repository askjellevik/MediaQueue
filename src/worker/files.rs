//! Which file of a torrent to select, and whether a finished torrent is the
//! one we asked for. Pure: the file list comes from Real-Debrid's `info`
//! answer and is re-checked here, whatever the candidate source claimed.
//!
//! Only one file is ever selected (never `all`), and a torrent that holds an
//! executable or script is rejected as a whole.

use super::rd::{InfoFile, TorrentInfo, TorrentStatus};

const MIB: u64 = 1024 * 1024;

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "m4v", "avi", "mov", "ts", "m2ts", "webm"];
const UNSAFE_EXTENSIONS: &[&str] = &["exe", "msi", "bat", "cmd", "scr", "lnk", "js", "vbs", "sh"];
const ARCHIVE_EXTENSIONS: &[&str] = &["rar", "zip", "7z"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdFile {
    pub id: u64,
    pub path: String,
    pub bytes: u64,
}

impl From<&InfoFile> for RdFile {
    fn from(file: &InfoFile) -> Self {
        Self {
            id: file.id,
            path: file.path.clone(),
            bytes: file.bytes,
        }
    }
}

/// The files Real-Debrid lists for a torrent.
pub fn files_of(info: &TorrentInfo) -> Vec<RdFile> {
    info.files.iter().map(RdFile::from).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FileRules {
    /// A video smaller than this is not the movie (samples, trailers).
    pub min_video_bytes: u64,
    /// A second main video at least this large (as a share of the largest)
    /// makes the choice ambiguous: a second part, or a second cut.
    pub ambiguous_ratio: f64,
}

impl Default for FileRules {
    fn default() -> Self {
        Self {
            min_video_bytes: 100 * MIB,
            ambiguous_ratio: 0.3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Select this file id.
    One(u64),
    /// Two main videos with a part marker on one, or a second video of real
    /// size: a human decides.
    Ambiguous,
    /// Reject the torrent; the reason is a fixed phrase.
    Unsafe(&'static str),
    NoVideo,
}

pub fn choose(files: &[RdFile], rules: &FileRules) -> Choice {
    if files
        .iter()
        .any(|f| UNSAFE_EXTENSIONS.contains(&extension(&f.path).as_str()))
    {
        return Choice::Unsafe("contains an executable or script");
    }

    let mut videos: Vec<&RdFile> = files
        .iter()
        .filter(|f| is_main_video(&f.path, f.bytes, rules))
        .collect();
    // Largest first; equal sizes by id so the answer does not depend on order.
    videos.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.id.cmp(&b.id)));

    // A marker means something only next to a second main video: a film
    // called "Part 2" on its own is just a film.
    if videos.len() >= 2 && videos.iter().any(|f| has_part_marker(&f.path)) {
        return Choice::Ambiguous;
    }

    let Some(main) = videos.first() else {
        let archived: u64 = files
            .iter()
            .filter(|f| ARCHIVE_EXTENSIONS.contains(&extension(&f.path).as_str()))
            .map(|f| f.bytes)
            .fold(0, u64::saturating_add);
        return if archived >= rules.min_video_bytes {
            Choice::Unsafe("only archives, no video")
        } else {
            Choice::NoVideo
        };
    };
    if let Some(second) = videos.get(1)
        && second.bytes as f64 >= main.bytes as f64 * rules.ambiguous_ratio
    {
        return Choice::Ambiguous;
    }
    Choice::One(main.id)
}

/// A video of movie size that is not a sample: what a fresh add may pick
/// and what a library hit must show.
fn is_main_video(path: &str, bytes: u64, rules: &FileRules) -> bool {
    VIDEO_EXTENSIONS.contains(&extension(path).as_str())
        && bytes >= rules.min_video_bytes
        && !is_sample(path)
}

/// A folder called `sample` or `samples`, or a file name with "sample" in
/// it. A release folder that merely has the word in the film's title
/// (`Sample.Of.Love.1994`) hides nothing.
fn is_sample(path: &str) -> bool {
    let mut segments = path.split(['/', '\\']).rev();
    let name = segments.next().unwrap_or(path);
    name.to_ascii_lowercase().contains("sample")
        || segments
            .any(|dir| dir.eq_ignore_ascii_case("sample") || dir.eq_ignore_ascii_case("samples"))
}

/// True when Real-Debrid has the selected file of a movie-sized video, not
/// a sample, within the size cap: what makes a library hit count as "already
/// there". The same rules as for a fresh add.
pub fn has_selected_video(info: &TorrentInfo, rules: &FileRules, max_bytes: u64) -> bool {
    info.files
        .iter()
        .any(|f| f.selected && is_main_video(&f.path, f.bytes, rules) && f.bytes <= max_bytes)
}

/// Why a finished torrent is not accepted. The text goes to the portal's
/// `last_error`, so it holds no file names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VerifyFail {
    #[error("torrent is not downloaded")]
    NotDownloaded,
    #[error("download is not complete")]
    Incomplete,
    #[error("torrent hash differs from the one asked for")]
    HashMismatch,
    #[error("selected files are not exactly the chosen one")]
    WrongSelection,
    #[error("no download links")]
    NoLinks,
    #[error("chosen file is smaller than a movie")]
    TooSmall,
    #[error("chosen file is over the size cap")]
    TooLarge,
}

/// The checks before a job may be reported `done`. Everything is read from
/// Real-Debrid's answer; the source's idea of the size plays no part.
pub fn verify(
    info: &TorrentInfo,
    expected_hash: &str,
    file_id: u64,
    rules: &FileRules,
    max_bytes: u64,
) -> Result<(), VerifyFail> {
    if info.status != TorrentStatus::Downloaded {
        return Err(VerifyFail::NotDownloaded);
    }
    if info.progress < 100.0 {
        return Err(VerifyFail::Incomplete);
    }
    if !info.hash.eq_ignore_ascii_case(expected_hash) {
        return Err(VerifyFail::HashMismatch);
    }
    let Some(chosen) = info.files.iter().find(|f| f.id == file_id) else {
        return Err(VerifyFail::WrongSelection);
    };
    if !chosen.selected || info.files.iter().any(|f| f.selected && f.id != file_id) {
        return Err(VerifyFail::WrongSelection);
    }
    if info.links.is_empty() {
        return Err(VerifyFail::NoLinks);
    }
    if chosen.bytes < rules.min_video_bytes {
        return Err(VerifyFail::TooSmall);
    }
    if chosen.bytes > max_bytes {
        return Err(VerifyFail::TooLarge);
    }
    Ok(())
}

/// True when the file name carries a multi-part marker: `cd1`, `disc 2`,
/// `part.1`, `pt3`, in any case. The marker must start a word (`Departed`
/// has none) and be followed by a digit, with at most one separator between.
fn has_part_marker(path: &str) -> bool {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    let bytes = name.as_bytes();
    ["cd", "disc", "disk", "part", "pt"].iter().any(|word| {
        name.match_indices(word).any(|(at, _)| {
            let starts_word = at == 0 || !bytes[at - 1].is_ascii_alphabetic();
            let mut next = at + word.len();
            if bytes
                .get(next)
                .is_some_and(|b| matches!(b, b' ' | b'.' | b'_' | b'-'))
            {
                next += 1;
            }
            starts_word && bytes.get(next).is_some_and(u8::is_ascii_digit)
        })
    })
}

/// The text after the last dot of the last path segment, lowercase; empty
/// when the name has no dot.
fn extension(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.')
        .map_or_else(String::new, |(_, ext)| ext.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * MIB;
    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn file(id: u64, path: &str, bytes: u64) -> RdFile {
        RdFile {
            id,
            path: path.into(),
            bytes,
        }
    }

    fn pick(files: &[RdFile]) -> Choice {
        choose(files, &FileRules::default())
    }

    #[test]
    fn a_single_mkv_is_the_choice() {
        let files = [file(1, "/Example.Movie.1994.1080p.WEB-DL.AAC.mkv", 4 * GIB)];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn extras_next_to_the_movie_are_ignored() {
        let files = [
            file(1, "/Example.Movie.1994.1080p.WEB-DL.AAC.nfo", 2_000),
            file(2, "/Example.Movie.1994.1080p.WEB-DL.AAC.mkv", 4 * GIB),
            file(3, "/Example.Movie.1994.1080p.WEB-DL.AAC.srt", 80_000),
            file(4, "/poster.jpg", 300_000),
        ];
        assert_eq!(pick(&files), Choice::One(2));
    }

    #[test]
    fn two_parts_of_similar_size_are_ambiguous() {
        let files = [
            file(1, "/Example.Movie.1994.CD1.avi", 700 * MIB),
            file(2, "/Example.Movie.1994.CD2.avi", 690 * MIB),
        ];
        assert_eq!(pick(&files), Choice::Ambiguous);
    }

    #[test]
    fn equal_sizes_are_ambiguous_whatever_the_order() {
        let a = [file(1, "/a.mkv", GIB), file(2, "/b.mkv", GIB)];
        let b = [file(2, "/b.mkv", GIB), file(1, "/a.mkv", GIB)];
        assert_eq!(pick(&a), Choice::Ambiguous);
        assert_eq!(pick(&b), Choice::Ambiguous);
    }

    #[test]
    fn a_clearly_smaller_second_video_does_not_confuse_the_choice() {
        let files = [
            file(1, "/Example.Movie.1994.mkv", 4 * GIB),
            file(2, "/Extras/making-of.mkv", 1200 * MIB),
        ];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn the_ratio_edge_is_inclusive() {
        let files = [file(1, "/a.mkv", 1000 * MIB), file(2, "/b.mkv", 300 * MIB)];
        assert_eq!(pick(&files), Choice::Ambiguous);
        let files = [file(1, "/a.mkv", 1000 * MIB), file(2, "/b.mkv", 299 * MIB)];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn a_second_video_well_under_the_largest_still_counts_as_a_second_main_video() {
        let files = [
            file(1, "/Example.Movie.1994.avi", 700 * MIB),
            file(2, "/Example.Movie.1994.extended.avi", 520 * MIB),
        ];
        assert_eq!(pick(&files), Choice::Ambiguous);
    }

    #[test]
    fn a_part_marker_with_a_second_main_video_is_ambiguous_whatever_the_sizes() {
        // The second part is far below 30% of the first, so only the marker
        // can tell.
        let files = [
            file(1, "/Example.Movie.1994.CD1.avi", 700 * MIB),
            file(2, "/Example.Movie.1994.CD2.avi", 150 * MIB),
        ];
        assert_eq!(pick(&files), Choice::Ambiguous);
        for (first, second) in [
            (
                "/Example.Movie.1994.part1.mkv",
                "/Example.Movie.1994.part2.mkv",
            ),
            (
                "/Example.Movie.1994.Part.1.mkv",
                "/Example.Movie.1994.Part.2.mkv",
            ),
            ("/Example.Movie.1994.pt1.mkv", "/Example.Movie.1994.pt2.mkv"),
            (
                "/Example Movie 1994 Disc 1.mkv",
                "/Example Movie 1994 Disc 2.mkv",
            ),
            (
                "/Example.Movie.1994-CD_1.avi",
                "/Example.Movie.1994-CD_2.avi",
            ),
        ] {
            let files = [file(1, first, 2 * GIB), file(2, second, 200 * MIB)];
            assert_eq!(pick(&files), Choice::Ambiguous, "{first}");
        }
    }

    #[test]
    fn a_single_video_named_like_a_part_is_just_the_film() {
        for name in [
            "/The.Example.Saga.Part.2.2015.1080p.mkv",
            "/The Example Saga - Part Two (2015).mkv",
            "/The.Example.Saga.Part2.2015.mkv",
            "/Example.Movie.1994.CD1.avi",
            "/Example.Movie.1994.disc1.mkv",
        ] {
            assert_eq!(pick(&[file(1, name, 2 * GIB)]), Choice::One(1), "{name}");
        }
        // Small extras under the size minimum do not make it a pair.
        let files = [
            file(1, "/The.Example.Saga.Part.2.2015.mkv", 4 * GIB),
            file(2, "/Extras/Part 1 recap.mkv", 80 * MIB),
        ];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn words_that_merely_contain_a_marker_are_not_parts() {
        for name in [
            "/The.Departed.2006.mkv",
            "/Example.Movie.1994.1080p.mkv",
            "/Apart.1994.mkv",
            "/Example.Movie.Part.Two.mkv",
        ] {
            assert_eq!(pick(&[file(1, name, 2 * GIB)]), Choice::One(1), "{name}");
        }
        // A marker on a folder is not on the file.
        let files = [file(1, "/CD1/Example.Movie.1994.mkv", 2 * GIB)];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn a_small_featurette_next_to_the_movie_is_not_a_second_main_video() {
        let files = [
            file(1, "/Example.Movie.1994.mkv", 4 * GIB),
            file(2, "/Extras/Featurette.mkv", 400 * MIB),
            file(3, "/Extras/Trailer.mkv", 80 * MIB),
        ];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn a_sample_is_ignored_in_any_case_and_any_directory() {
        let files = [
            file(1, "/Example.Movie.1994.mkv", 4 * GIB),
            file(2, "/Sample/example.movie.1994.sample.mkv", 3 * GIB),
            file(3, "/Example.Movie.1994-SAMPLE.mkv", 3 * GIB),
        ];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn a_short_video_is_not_the_movie() {
        let files = [file(1, "/trailer.mp4", 99 * MIB)];
        assert_eq!(pick(&files), Choice::NoVideo);
        let files = [file(1, "/clip.mp4", 100 * MIB)];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn executables_and_scripts_anywhere_reject_the_torrent() {
        for name in [
            "/setup.exe",
            "/Example.Movie.1994.mkv.exe",
            "/run.sh",
            "/Example.Movie.lnk",
            "/a/b/c/install.msi",
            "/go.bat",
            "/go.cmd",
            "/x.scr",
            "/x.js",
            "/x.vbs",
            "/INSTALL.EXE",
        ] {
            let files = [
                file(1, "/Example.Movie.1994.mkv", 4 * GIB),
                file(2, name, 5_000),
            ];
            assert!(matches!(pick(&files), Choice::Unsafe(_)), "{name}");
        }
    }

    #[test]
    fn a_safe_double_extension_is_not_mistaken_for_an_executable() {
        let files = [
            file(1, "/Example.Movie.1994.exe.mkv", 4 * GIB),
            file(2, "/notes.sh.txt", 100),
        ];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn archives_without_a_video_are_unsafe_but_small_ones_are_just_no_video() {
        let files = [
            file(1, "/movie.part1.rar", 3 * GIB),
            file(2, "/movie.part2.rar", 3 * GIB),
        ];
        assert!(matches!(pick(&files), Choice::Unsafe(_)));
        let files = [file(1, "/subs.zip", 2 * MIB), file(2, "/info.nfo", 10)];
        assert_eq!(pick(&files), Choice::NoVideo);
        // A video wins over an archive next to it.
        let files = [file(1, "/movie.mkv", 4 * GIB), file(2, "/extras.rar", GIB)];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn only_an_nfo_and_an_empty_list_are_no_video() {
        assert_eq!(pick(&[file(1, "/info.nfo", 1_000)]), Choice::NoVideo);
        assert_eq!(pick(&[]), Choice::NoVideo);
    }

    #[test]
    fn extensions_are_case_insensitive() {
        let files = [file(1, "/EXAMPLE.MOVIE.1994.MKV", 4 * GIB)];
        assert_eq!(pick(&files), Choice::One(1));
        let files = [file(1, "/Example.Movie.1994.Mp4", 4 * GIB)];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn dots_in_directories_and_missing_extensions_do_not_confuse_it() {
        assert_eq!(extension("/Example.Movie.1994.mkv/readme"), "");
        assert_eq!(extension("/a.b.c/movie.MKV"), "mkv");
        assert_eq!(extension("/a.exe/movie"), "");
        assert_eq!(extension("\\a\\movie.mp4"), "mp4");
        assert_eq!(extension("/trailing."), "");
        let files = [
            file(
                1,
                "/Example.Movie.1994.1080p.x264.exe.dir/movie.mkv",
                4 * GIB,
            ),
            file(2, "/Example.Movie.1994.1080p.x264.exe.dir/readme", 100),
        ];
        assert_eq!(pick(&files), Choice::One(1));
    }

    #[test]
    fn every_video_extension_counts() {
        for ext in ["mkv", "mp4", "m4v", "avi", "mov", "ts", "m2ts", "webm"] {
            let files = [file(1, &format!("/movie.{ext}"), GIB)];
            assert_eq!(pick(&files), Choice::One(1), "{ext}");
        }
        assert_eq!(pick(&[file(1, "/movie.iso", 4 * GIB)]), Choice::NoVideo);
    }

    fn done_info() -> TorrentInfo {
        TorrentInfo {
            id: "ABCDEFGHIJKLM".into(),
            hash: HASH.into(),
            status: TorrentStatus::Downloaded,
            progress: 100.0,
            bytes: 4 * GIB,
            files: vec![
                InfoFile {
                    id: 1,
                    path: "/Example.Movie.1994.mkv".into(),
                    bytes: 4 * GIB,
                    selected: true,
                },
                InfoFile {
                    id: 2,
                    path: "/info.nfo".into(),
                    bytes: 1_200,
                    selected: false,
                },
            ],
            links: vec!["https://example.invalid/d/1".into()],
        }
    }

    fn check(info: &TorrentInfo) -> Result<(), VerifyFail> {
        verify(info, HASH, 1, &FileRules::default(), 15 * GIB)
    }

    #[test]
    fn a_finished_torrent_passes() {
        assert_eq!(check(&done_info()), Ok(()));
        assert_eq!(
            verify(
                &done_info(),
                &HASH.to_uppercase(),
                1,
                &FileRules::default(),
                4 * GIB
            ),
            Ok(())
        );
    }

    #[test]
    fn verify_fails_one_check_at_a_time() {
        let mut info = done_info();
        info.status = TorrentStatus::Downloading;
        assert_eq!(check(&info), Err(VerifyFail::NotDownloaded));

        let mut info = done_info();
        info.progress = 99.0;
        assert_eq!(check(&info), Err(VerifyFail::Incomplete));

        let mut info = done_info();
        info.hash = "b".repeat(40);
        assert_eq!(check(&info), Err(VerifyFail::HashMismatch));

        let mut info = done_info();
        info.files[1].selected = true;
        assert_eq!(check(&info), Err(VerifyFail::WrongSelection));

        let mut info = done_info();
        info.files[0].selected = false;
        assert_eq!(check(&info), Err(VerifyFail::WrongSelection));

        let mut info = done_info();
        info.files.clear();
        assert_eq!(check(&info), Err(VerifyFail::WrongSelection));

        let mut info = done_info();
        info.links.clear();
        assert_eq!(check(&info), Err(VerifyFail::NoLinks));

        let mut info = done_info();
        info.files[0].bytes = 99 * MIB;
        assert_eq!(check(&info), Err(VerifyFail::TooSmall));

        let mut info = done_info();
        info.files[0].bytes = 15 * GIB + 1;
        assert_eq!(check(&info), Err(VerifyFail::TooLarge));
    }

    #[test]
    fn the_size_bounds_are_inclusive() {
        let mut info = done_info();
        info.files[0].bytes = 100 * MIB;
        assert_eq!(check(&info), Ok(()));
        info.files[0].bytes = 15 * GIB;
        assert_eq!(check(&info), Ok(()));
    }

    #[test]
    fn a_selected_movie_sized_video_is_what_makes_a_hit_count() {
        let rules = FileRules::default();
        let hit = |info: &TorrentInfo| has_selected_video(info, &rules, 15 * GIB);
        assert!(hit(&done_info()));

        let mut info = done_info();
        info.files[0].selected = false;
        assert!(!hit(&info));

        let mut info = done_info();
        info.files[0].bytes = 99 * MIB;
        assert!(!hit(&info));

        let mut info = done_info();
        info.files[1].selected = true;
        info.files[0].selected = false;
        assert!(!hit(&info), "an nfo is not a video");
    }

    #[test]
    fn a_library_hit_follows_the_sample_and_size_rules_of_a_fresh_add() {
        let rules = FileRules::default();
        let hit = |info: &TorrentInfo| has_selected_video(info, &rules, 15 * GIB);

        let mut info = done_info();
        info.files[0].path = "/Sample/example.movie.1994.mkv".into();
        assert!(!hit(&info), "a sample folder");
        info.files[0].path = "/Example.Movie.1994-sample.mkv".into();
        assert!(!hit(&info), "a sample file");

        let mut info = done_info();
        info.files[0].bytes = 15 * GIB;
        assert!(hit(&info), "the cap is inclusive");
        info.files[0].bytes = 15 * GIB + 1;
        assert!(!hit(&info), "over the cap");
    }

    #[test]
    fn a_release_folder_with_sample_in_the_title_does_not_hide_the_file() {
        let files = [file(
            1,
            "/Sample.Of.Love.1994.1080p.WEB-DL/Sample.Of.Love.1994.1080p.mkv",
            4 * GIB,
        )];
        // The file name itself has the word: skipped, as for any sample.
        assert_eq!(pick(&files), Choice::NoVideo);
        let files = [
            file(
                1,
                "/Sample.Of.Love.1994.1080p.WEB-DL/Love.1994.1080p.mkv",
                4 * GIB,
            ),
            file(
                2,
                "/Sample.Of.Love.1994.1080p.WEB-DL/Sample/love-sample.mkv",
                300 * MIB,
            ),
        ];
        assert_eq!(pick(&files), Choice::One(1));
        let mut info = done_info();
        info.files[0].path = "/Sample.Of.Love.1994.1080p.WEB-DL/Love.1994.1080p.mkv".into();
        assert!(has_selected_video(&info, &FileRules::default(), 15 * GIB));
    }

    #[test]
    fn only_a_folder_named_sample_or_samples_counts_as_a_sample_folder() {
        for (path, sample) in [
            ("/Sample/movie.mkv", true),
            ("/Samples/movie.mkv", true),
            ("/release/SAMPLES/movie.mkv", true),
            ("\\release\\Sample\\movie.mkv", true),
            ("/release/movie.sample.mkv", true),
            ("/release/SAMPLE-movie.mkv", true),
            ("/Sampler/movie.mkv", false),
            ("/The.Sample.Case.1994/movie.mkv", false),
            ("/release/movie.mkv", false),
        ] {
            assert_eq!(is_sample(path), sample, "{path}");
        }
    }

    #[test]
    fn verify_messages_carry_no_names() {
        for fail in [
            VerifyFail::NotDownloaded,
            VerifyFail::Incomplete,
            VerifyFail::HashMismatch,
            VerifyFail::WrongSelection,
            VerifyFail::NoLinks,
            VerifyFail::TooSmall,
            VerifyFail::TooLarge,
        ] {
            assert!(!fail.to_string().contains('/'));
        }
    }

    #[test]
    fn files_of_copies_id_path_and_size() {
        let files = files_of(&done_info());
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], file(1, "/Example.Movie.1994.mkv", 4 * GIB));
    }
}
