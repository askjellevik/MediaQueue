//! Reads the text Torrentio puts in a stream's `title` and `name` fields.
//!
//! A `title` is two to four lines, and the layout shifts, so the statistics
//! line is found by its 👤 marker and never by position:
//!
//! ```text
//! <release name>
//! 👤 <seeders> 💾 <size> ⚙️ <provider>
//! <languages>                    (optional: flags and "Multi Audio")
//!
//! <pack name>                    (a pack: the name of the pack,
//! <file inside the pack>          then the file the stream points at)
//! 👤 ... 💾 ... ⚙️ ...
//! <languages>
//! ```
//!
//! The `name` field is `Torrentio` and, on a second line, a quality label.

use super::candidate::RawCandidate;

const LONGEST_PROVIDER: usize = 100;

/// What could be read from one stream's text.
#[derive(Debug, Clone, PartialEq)]
pub struct TextParts {
    pub name: String,
    pub filename: Option<String>,
    pub pack: bool,
    pub label: Option<String>,
    pub seeders: Option<u32>,
    pub size_bytes: Option<u64>,
    pub provider: Option<String>,
    pub flag_languages: Vec<String>,
    pub multi_audio: bool,
}

impl TextParts {
    pub fn into_candidate(self, infohash: String) -> RawCandidate {
        RawCandidate {
            infohash,
            name: self.name,
            filename: self.filename,
            pack: self.pack,
            label: self.label,
            size_bytes: self.size_bytes,
            seeders: self.seeders,
            provider: self.provider,
            flag_languages: self.flag_languages,
            multi_audio: self.multi_audio,
        }
    }
}

/// `None` when the title has no statistics line or no name before it: the
/// caller drops such an item.
pub fn parse_stream_text(
    name_field: &str,
    title_field: &str,
    filename: Option<&str>,
) -> Option<TextParts> {
    // Split on the line breaks first; stripping control characters first
    // would glue the lines together.
    let lines: Vec<String> = title_field
        .split('\n')
        .map(|line| line.chars().filter(|c| !c.is_control()).collect())
        .collect();
    let stats_at = lines.iter().position(|line| line.contains('👤'))?;

    let names: Vec<&str> = lines[..stats_at]
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    let name = (*names.first()?).to_string();
    let pack = names.len() >= 2;
    let filename = filename
        .map(|f| f.chars().filter(|c| !c.is_control()).collect::<String>())
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty())
        .or_else(|| pack.then(|| names[1].to_string()));

    let stats = &lines[stats_at];
    let (flag_languages, multi_audio) = languages(&lines[stats_at + 1..]);

    Some(TextParts {
        name,
        filename,
        pack,
        label: label(name_field),
        seeders: seeders(stats),
        size_bytes: size_bytes(stats),
        provider: provider(stats),
        flag_languages,
        multi_audio,
    })
}

/// The second line of the `name` field, such as `4k DV | HDR`.
fn label(name_field: &str) -> Option<String> {
    let line: String = name_field
        .split('\n')
        .nth(1)?
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let line = line.trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// The text right after a marker character.
fn after(line: &str, marker: char) -> Option<&str> {
    let at = line.find(marker)?;
    Some(&line[at + marker.len_utf8()..])
}

fn seeders(stats: &str) -> Option<u32> {
    let rest = after(stats, '👤')?.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `💾 6.91 GB`. Torrentio's figures are binary (a GiB), so the units are
/// powers of 1024.
fn size_bytes(stats: &str) -> Option<u64> {
    let rest = after(stats, '💾')?;
    let mut words = rest.split_whitespace();
    let value: f64 = words.next()?.parse().ok()?;
    let unit: u32 = match words.next()?.to_ascii_uppercase().as_str() {
        "KB" => 1,
        "MB" => 2,
        "GB" => 3,
        "TB" => 4,
        _ => return None,
    };
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    // Saturates on absurd values, which the size filter then drops.
    Some((value * 1024f64.powi(unit as i32)) as u64)
}

fn provider(stats: &str) -> Option<String> {
    let rest = after(stats, '\u{2699}')?;
    let rest = rest.trim_start_matches('\u{FE0F}').trim();
    (!rest.is_empty()).then(|| rest.chars().take(LONGEST_PROVIDER).collect())
}

/// Country flags are two regional-indicator characters.
fn flag_code(token: &str) -> Option<String> {
    let mut code = String::new();
    for c in token.chars() {
        let n = c as u32;
        if !(0x1F1E6..=0x1F1FF).contains(&n) {
            return None;
        }
        code.push(char::from(b'a' + (n - 0x1F1E6) as u8));
    }
    (code.len() == 2).then_some(code)
}

/// The language a country flag stands for. A flag with no entry is kept as
/// its country code, which still counts as "not English".
fn language_of_country(country: &str) -> &str {
    match country {
        "gb" | "us" | "au" | "ca" | "ie" | "nz" => "en",
        "ru" => "ru",
        "it" => "it",
        "fr" => "fr",
        "de" => "de",
        "es" | "mx" | "ar" | "co" | "cl" => "es",
        "pt" | "br" => "pt",
        "in" => "hi",
        "jp" => "ja",
        "kr" => "ko",
        "cn" | "tw" => "zh",
        "nl" => "nl",
        "pl" => "pl",
        "cz" => "cs",
        "dk" => "da",
        "se" => "sv",
        "no" => "no",
        "ua" => "uk",
        "hu" => "hu",
        other => other,
    }
}

fn languages(lines: &[String]) -> (Vec<String>, bool) {
    let mut codes: Vec<String> = Vec::new();
    let mut multi_audio = false;
    for line in lines {
        for token in line.split(" / ") {
            let token = token.trim();
            let lower = token.to_lowercase();
            if lower == "multi audio" || lower == "dual audio" {
                multi_audio = true;
            } else if let Some(country) = flag_code(token) {
                let code = language_of_country(&country).to_string();
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
    }
    (codes, multi_audio)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every title here is invented, in Torrentio's layout.

    #[test]
    fn a_two_line_title_has_a_name_and_the_statistics() {
        let parts = parse_stream_text(
            "Torrentio\n1080p",
            "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE\n👤 120 💾 2.5 GB ⚙️ ExampleSite",
            Some("Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE.mkv"),
        )
        .unwrap();
        assert_eq!(
            parts.name,
            "Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE"
        );
        assert_eq!(
            parts.filename.as_deref(),
            Some("Example.Movie.1994.1080p.BluRay.x264.AAC-GRPONE.mkv")
        );
        assert!(!parts.pack);
        assert_eq!(parts.label.as_deref(), Some("1080p"));
        assert_eq!(parts.seeders, Some(120));
        assert_eq!(parts.size_bytes, Some((2.5 * 1024f64.powi(3)) as u64));
        assert_eq!(parts.provider.as_deref(), Some("ExampleSite"));
        assert!(parts.flag_languages.is_empty() && !parts.multi_audio);
    }

    #[test]
    fn a_third_line_holds_the_language_flags() {
        let parts = parse_stream_text(
            "Torrentio\n1080p",
            "Example Movie (1994) 1080p x264\n👤 5 💾 3.53 GB ⚙️ ExampleSite\n🇬🇧 / 🇫🇷",
            None,
        )
        .unwrap();
        assert_eq!(parts.flag_languages, ["en", "fr"]);
        assert_eq!(parts.filename, None, "a normal title has no inner file");
    }

    #[test]
    fn a_pack_has_two_name_lines_and_the_inner_file_is_the_filename() {
        let parts = parse_stream_text(
            "Torrentio\n1080p",
            "Example Writer Pack (1985 - 2003)\nExample Movie (1994) 1080p.mkv\n👤 9 💾 1.09 GB ⚙️ ExampleSite\n🇬🇧",
            None,
        )
        .unwrap();
        assert!(parts.pack);
        assert_eq!(parts.name, "Example Writer Pack (1985 - 2003)");
        assert_eq!(
            parts.filename.as_deref(),
            Some("Example Movie (1994) 1080p.mkv")
        );
        assert_eq!(parts.flag_languages, ["en"]);
        // A real filename from the stream wins over the inner line.
        let with_file = parse_stream_text(
            "Torrentio",
            "Example Writer Pack\nExample Movie (1994) 1080p.mkv\n👤 9 💾 1.09 GB ⚙️ ExampleSite",
            Some("Example Movie (1994).mkv"),
        )
        .unwrap();
        assert_eq!(
            with_file.filename.as_deref(),
            Some("Example Movie (1994).mkv")
        );
    }

    #[test]
    fn a_cyrillic_title_keeps_its_text_and_both_flags() {
        let parts = parse_stream_text(
            "Torrentio\n1080p",
            "Пример Фильма / Example Movie [1994 BDRip 1080p]\n👤 18 💾 16.96 GB ⚙️ ExampleTracker\n🇬🇧 / 🇷🇺",
            None,
        )
        .unwrap();
        assert_eq!(
            parts.name,
            "Пример Фильма / Example Movie [1994 BDRip 1080p]"
        );
        assert_eq!(parts.flag_languages, ["en", "ru"]);
    }

    #[test]
    fn sizes_are_binary_and_may_be_in_megabytes() {
        let size = |stats: &str| {
            parse_stream_text("Torrentio", &format!("Example\n{stats}"), None)
                .unwrap()
                .size_bytes
        };
        assert_eq!(
            size("👤 5 💾 702.96 MB ⚙️ ExampleSite"),
            Some((702.96 * 1024f64.powi(2)) as u64)
        );
        assert_eq!(
            size("👤 5 💾 6.91 GB ⚙️ ExampleSite"),
            Some((6.91 * 1024f64.powi(3)) as u64)
        );
        assert_eq!(size("👤 5 💾 1 TB ⚙️ ExampleSite"), Some(1 << 40));
        assert_eq!(size("👤 5 💾 abc GB ⚙️ ExampleSite"), None);
        assert_eq!(size("👤 5 💾 3 parsecs ⚙️ ExampleSite"), None);
        assert_eq!(size("👤 5 ⚙️ ExampleSite"), None);
    }

    #[test]
    fn multi_audio_and_unknown_flags_are_understood() {
        let parts = parse_stream_text(
            "Torrentio",
            "Example\n👤 5 💾 1 GB ⚙️ ExampleSite\nMulti Audio / 🇬🇧 / 🇮🇹 / 🇹🇷",
            None,
        )
        .unwrap();
        assert!(parts.multi_audio);
        assert_eq!(parts.flag_languages, ["en", "it", "tr"]);
        let dual = parse_stream_text(
            "Torrentio",
            "Example\n👤 5 💾 1 GB ⚙️ ExampleSite\nDual Audio",
            None,
        )
        .unwrap();
        assert!(dual.multi_audio && dual.flag_languages.is_empty());
        // "Multi Subs" says nothing about the audio.
        let subs = parse_stream_text(
            "Torrentio",
            "Example\n👤 5 💾 1 GB ⚙️ ExampleSite\nMulti Subs / 🇬🇧",
            None,
        )
        .unwrap();
        assert!(!subs.multi_audio);
    }

    #[test]
    fn the_label_is_the_second_line_of_the_name_field() {
        let label = |name: &str| {
            parse_stream_text(name, "Example\n👤 5 💾 1 GB ⚙️ ExampleSite", None)
                .unwrap()
                .label
        };
        assert_eq!(
            label("Torrentio\n4k DV | HDR").as_deref(),
            Some("4k DV | HDR")
        );
        assert_eq!(label("Torrentio"), None);
        assert_eq!(label("Torrentio\n"), None);
        assert_eq!(label(""), None);
    }

    #[test]
    fn a_title_without_statistics_or_a_name_is_refused() {
        assert_eq!(
            parse_stream_text("Torrentio", "Example.Movie.1994", None),
            None
        );
        assert_eq!(parse_stream_text("Torrentio", "", None), None);
        assert_eq!(
            parse_stream_text("Torrentio", "👤 5 💾 1 GB ⚙️ ExampleSite", None),
            None,
            "no name before the statistics"
        );
        assert_eq!(
            parse_stream_text("Torrentio", "\n\n👤 5 💾 1 GB ⚙️ ExampleSite", None),
            None
        );
    }

    #[test]
    fn control_characters_are_removed_but_the_lines_stay_apart() {
        let parts = parse_stream_text(
            "Torrentio\r\n1080p",
            "Example\u{7}.Movie\r\n👤 5 💾 1 GB ⚙️ Example\u{0}Site\r\n🇬🇧",
            Some("Example\u{1b}.mkv"),
        )
        .unwrap();
        assert_eq!(parts.name, "Example.Movie");
        assert_eq!(parts.provider.as_deref(), Some("ExampleSite"));
        assert_eq!(parts.filename.as_deref(), Some("Example.mkv"));
        assert_eq!(parts.flag_languages, ["en"]);
        assert_eq!(parts.label.as_deref(), Some("1080p"));
    }

    #[test]
    fn missing_pieces_of_the_statistics_leave_the_rest_intact() {
        let parts = parse_stream_text("Torrentio", "Example\n👤 abc 💾 1 GB", None).unwrap();
        assert_eq!(parts.seeders, None);
        assert_eq!(parts.size_bytes, Some(1 << 30));
        assert_eq!(parts.provider, None);
    }

    #[test]
    fn the_candidate_carries_every_field() {
        let parts = parse_stream_text(
            "Torrentio\n1080p",
            "Example\n👤 7 💾 1 GB ⚙️ ExampleSite\nMulti Audio / 🇬🇧",
            Some("Example.mkv"),
        )
        .unwrap();
        let candidate = parts.into_candidate("0".repeat(40));
        assert_eq!(candidate.infohash, "0".repeat(40));
        assert_eq!(candidate.seeders, Some(7));
        assert_eq!(candidate.filename.as_deref(), Some("Example.mkv"));
        assert!(candidate.multi_audio);
        assert_eq!(candidate.flag_languages, ["en"]);
    }
}
