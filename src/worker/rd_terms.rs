//! The names Real-Debrid refuses (error 35, infringing file).
//!
//! Real-Debrid matches a name against a list of strings, as a plain
//! substring with the separator exactly as written: `WEB-DL` is refused,
//! `WEB DL` and `WEB-Rip` are not. The list comes from a third-party post
//! ("Complete List of Real-Debrid's Filename Filters" on DMM), reflects the
//! rules as of 14 October 2026, and may change; it is a setting, not a
//! constant of nature. Nothing is normalised here: that would drop names
//! Real-Debrid takes.

/// The default list, in the notation of `WORKER_RD_BLOCKED_TERMS`: comma
/// separated; `=` in front of a term makes it case-sensitive, `#` makes it
/// count only when a resolution (3 or 4 digits, then `p`, then a dot)
/// directly precedes it; any other term is case-insensitive.
pub const DEFAULT_TERMS: &str = "=WEB-DL,=WEB.x264,=WEB.H264,WEBRip,BDRip,HDRip,DVDRip,\
HDTV.x264,HDTV.XviD,BluRay.x264,#BluRay.DTS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedTerm {
    text: String,
    case_sensitive: bool,
    after_resolution: bool,
}

impl BlockedTerm {
    /// One entry of the list. `None` when nothing is left after the prefix.
    pub fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        let (text, case_sensitive, after_resolution) = if let Some(rest) = entry.strip_prefix('=') {
            (rest, true, false)
        } else if let Some(rest) = entry.strip_prefix('#') {
            (rest, false, true)
        } else {
            (entry, false, false)
        };
        (!text.is_empty()).then(|| Self {
            text: text.to_string(),
            case_sensitive,
            after_resolution,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Does `name` contain the term, anywhere, by plain substring match?
    pub fn matches(&self, name: &str) -> bool {
        if self.case_sensitive {
            return name.contains(&self.text);
        }
        let name = name.to_ascii_lowercase();
        let term = self.text.to_ascii_lowercase();
        if self.after_resolution {
            name.match_indices(&term)
                .any(|(at, _)| resolution_before(&name.as_bytes()[..at]))
        } else {
            name.contains(&term)
        }
    }
}

/// `head` ends in a resolution and a dot: 3 or 4 digits, `p`, `.`. A longer
/// run of digits is no resolution, and neither is a year.
fn resolution_before(head: &[u8]) -> bool {
    let Some(digits) = head.strip_suffix(b"p.") else {
        return false;
    };
    let run = digits
        .iter()
        .rev()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    (3..=4).contains(&run)
}

/// The list `text` spells out, blanks and empty entries skipped. An entry
/// with a prefix and nothing after it is an error, the entry itself.
pub fn parse_list(text: &str) -> Result<Vec<BlockedTerm>, String> {
    text.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| BlockedTerm::parse(entry).ok_or_else(|| entry.to_string()))
        .collect()
}

pub fn default_terms() -> Vec<BlockedTerm> {
    parse_list(DEFAULT_TERMS).unwrap_or_default()
}

/// Does `name` contain any of the terms?
pub fn any_match(terms: &[BlockedTerm], name: &str) -> bool {
    terms.iter().any(|term| term.matches(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every name here is invented.

    fn term(entry: &str) -> BlockedTerm {
        BlockedTerm::parse(entry).unwrap()
    }

    fn blocked(name: &str) -> bool {
        any_match(&default_terms(), name)
    }

    #[test]
    fn the_default_list_has_all_eleven_terms() {
        assert_eq!(default_terms().len(), 11);
    }

    #[test]
    fn a_case_sensitive_term_ignores_other_spellings() {
        let web_dl = term("=WEB-DL");
        assert!(web_dl.matches("Example.Movie.1994.1080p.WEB-DL.AAC"));
        assert!(!web_dl.matches("Example.Movie.1994.1080p.web-dl.AAC"));
        assert!(!web_dl.matches("Example.Movie.1994.1080p.Web-Dl.AAC"));
    }

    #[test]
    fn a_term_without_a_prefix_ignores_case() {
        let rip = term("WEBRip");
        assert!(rip.matches("Example.Movie.1994.WEBRip.AAC"));
        assert!(rip.matches("Example.Movie.1994.webrip.AAC"));
        assert!(rip.matches("Example.Movie.1994.WebRIP.AAC"));
    }

    #[test]
    fn a_term_counts_anywhere_in_the_name_and_whatever_the_extension() {
        assert!(term("BDRip").matches("BDRip"));
        assert!(term("BDRip").matches("Example.Movie.1994.BDRip.mkv"));
        assert!(term("BDRip").matches("Example.Movie.1994.1080p.XBDRipper.AAC"));
    }

    #[test]
    fn the_hash_prefix_needs_a_resolution_directly_before() {
        let dts = term("#BluRay.DTS");
        assert!(dts.matches("Example.Movie.1994.1080p.BluRay.DTS-GRP"));
        assert!(dts.matches("Example.Movie.1994.2160p.BluRay.DTS-HD.MA.5.1"));
        assert!(dts.matches("Example.Movie.1994.720P.BLURAY.DTS"));
        assert!(dts.matches("Example.Movie.480p.BluRay.DTS"));
        assert!(!dts.matches("Example.Movie.1994.REMUX.BluRay.DTS"));
        assert!(!dts.matches("Example.Movie.1994.BluRay.1080p.DTS"));
        assert!(!dts.matches("Example.Movie.1994.BluRay.DTS"));
        assert!(!dts.matches("BluRay.DTS"));
        // Five digits are no resolution; neither is a digit run without a "p".
        assert!(!dts.matches("Example.Movie.10800p.BluRay.DTS"));
        assert!(!dts.matches("Example.Movie.1080.BluRay.DTS"));
        assert!(!dts.matches("Example.Movie.80p.BluRay.DTS"));
        // One occurrence with a resolution is enough.
        assert!(dts.matches("Example.1994.BluRay.DTS.1080p.BluRay.DTS"));
    }

    #[test]
    fn the_equals_prefix_is_not_the_hash_prefix_and_not_part_of_the_text() {
        let term = term("=1080p.BluRay.DTS");
        assert!(term.matches("Example.1080p.BluRay.DTS"));
        assert!(!term.matches("Example.1080p.bluray.dts"));
        assert_eq!(term.text(), "1080p.BluRay.DTS");
    }

    #[test]
    fn separators_are_not_normalised() {
        for name in [
            "Example.Movie.1994.1080p.WEB DL.AAC",
            "Example.Movie.1994.1080p.WEB_DL.AAC",
            "Example.Movie.1994.1080p.WEB.DL.AAC",
            "Example.Movie.1994.1080p.WEB-Rip.AAC",
            "Example.Movie.1994.HDTV-XviD",
            "Example.Movie.1994.HDTV XviD",
            "Example.Movie.1994.1080p.BluRay-x264",
            "Example.Movie.1994.1080p.BluRay x264",
            "Example.Movie.1994.1080p.BluRay-DTS",
            "Example.Movie.1994.1080p BluRay.DTS",
            "Example.Movie.1994.1080p-BluRay.DTS",
        ] {
            assert!(!blocked(name), "{name}");
        }
    }

    #[test]
    fn the_default_list_follows_the_post() {
        for name in [
            "Example.Movie.1994.1080p.WEB-DL.AAC",
            "Example.Movie.1994.1080p.WEB.x264.AAC",
            "Example.Movie.1994.1080p.WEB.H264.AAC",
            "Example.Movie.1994.1080p.WEBRip.AAC",
            "Example.Movie.1994.BDRip.AAC",
            "Example.Movie.1994.HDRip.AAC",
            "Example.Movie.1994.DVDRip.AAC",
            "Example.Movie.1994.HDTV.x264.AAC",
            "Example.Movie.1994.HDTV.XviD.AAC",
            "Example.Movie.1994.1080p.BluRay.x264.AAC",
            "Example.Movie.1994.2160p.BluRay.DTS-HD.MA",
        ] {
            assert!(blocked(name), "{name}");
        }
        for name in [
            "Example.Movie.1994.1080p.web-dl.AAC",
            "Example.Movie.1994.1080p.WEB.h264.AAC",
            "Example.Movie.1994.1080p.BluRay.x265.AAC",
            "Example.Movie.1994.1080p.BluRay.h264.AAC",
            "Example.Movie.1994.1080p.REMUX.AVC.AAC",
            "Example.Movie.1994.1080p.AMZN.NF.YTS.RARBG",
            "Example.Movie.1994.REMUX.BluRay.DTS",
            "Example.Movie.1994.BluRay.1080p.DTS",
        ] {
            assert!(!blocked(name), "{name}");
        }
    }

    #[test]
    fn the_list_is_comma_separated_and_blanks_are_skipped() {
        let list = parse_list(" =Abc , def,, #ghi ,").unwrap();
        assert_eq!(
            list,
            [term("=Abc"), term("def"), term("#ghi")],
            "terms are trimmed inside, never normalised"
        );
        assert!(parse_list("").unwrap().is_empty());
        assert!(parse_list(" , ").unwrap().is_empty());
        assert_eq!(parse_list("WEB DL").unwrap()[0].text(), "WEB DL");
        assert_eq!(parse_list("=").unwrap_err(), "=");
        assert_eq!(parse_list("abc,#").unwrap_err(), "#");
    }
}
