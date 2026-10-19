//! Reads a release name ("Some.Film.1994.1080p.BluRay.x264.AAC5.1-GROUP")
//! into the tags the worker cares about: resolution, source, video codec,
//! audio codecs, Dolby Vision, years, languages and a few warning words.
//!
//! The text is lowercased, accents folded to ASCII and split into tokens on
//! every character that is not a letter or digit. Aliases are then matched as
//! whole-token sequences (`["dts", "hd", "ma"]`), never as substrings, so
//! `ts` does not match inside `ghosts` and `dd` not inside `add`.

/// Pixel rows. The order is the order of preference for "the highest one".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Resolution {
    Unknown,
    Sd,
    R720,
    R1080,
    R1440,
    R2160,
}

impl Resolution {
    /// 2160, 1080 and so on; 0 for unknown and 480 for anything SD.
    pub fn lines(self) -> u32 {
        match self {
            Resolution::Unknown => 0,
            Resolution::Sd => 480,
            Resolution::R720 => 720,
            Resolution::R1080 => 1080,
            Resolution::R1440 => 1440,
            Resolution::R2160 => 2160,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Source {
    Unknown,
    Cam,
    Telesync,
    Screener,
    Dvd,
    Hdrip,
    Hdtv,
    WebRip,
    WebDl,
    BluRay,
    Remux,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Unknown => "unknown",
            Source::Cam => "cam",
            Source::Telesync => "telesync",
            Source::Screener => "screener",
            Source::Dvd => "dvd",
            Source::Hdrip => "hdrip",
            Source::Hdtv => "hdtv",
            Source::WebRip => "webrip",
            Source::WebDl => "web-dl",
            Source::BluRay => "bluray",
            Source::Remux => "remux",
        }
    }

    /// A cinema recording or a pre-release copy.
    pub fn is_poor_copy(self) -> bool {
        matches!(self, Source::Cam | Source::Telesync | Source::Screener)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Video {
    Unknown,
    H264,
    H265,
    Av1,
    Xvid,
    Divx,
    Mpeg2,
    Vc1,
}

impl Video {
    pub fn as_str(self) -> &'static str {
        match self {
            Video::Unknown => "unknown",
            Video::H264 => "h264",
            Video::H265 => "h265",
            Video::Av1 => "av1",
            Video::Xvid => "xvid",
            Video::Divx => "divx",
            Video::Mpeg2 => "mpeg2",
            Video::Vc1 => "vc1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Hdr {
    None,
    Hdr,
    Hdr10Plus,
    Hlg,
}

impl Hdr {
    pub fn as_str(self) -> &'static str {
        match self {
            Hdr::None => "none",
            Hdr::Hdr => "hdr",
            Hdr::Hdr10Plus => "hdr10plus",
            Hdr::Hlg => "hlg",
        }
    }
}

/// An audio codec named in the release. The declaration order is the order
/// the tags are stored and printed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Audio {
    Aac,
    Ac3,
    Eac3,
    Dts,
    DtsHd,
    TrueHd,
    Atmos,
    Flac,
    Lpcm,
    Opus,
    Mp3,
    Vorbis,
}

impl Audio {
    pub fn as_str(self) -> &'static str {
        match self {
            Audio::Aac => "aac",
            Audio::Ac3 => "ac3",
            Audio::Eac3 => "eac3",
            Audio::Dts => "dts",
            Audio::DtsHd => "dtshd",
            Audio::TrueHd => "truehd",
            Audio::Atmos => "atmos",
            Audio::Flac => "flac",
            Audio::Lpcm => "lpcm",
            Audio::Opus => "opus",
            Audio::Mp3 => "mp3",
            Audio::Vorbis => "vorbis",
        }
    }
}

/// A spoken language named in the release, or a "several languages" marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    English,
    Italian,
    French,
    German,
    Spanish,
    Russian,
    Hindi,
    Norwegian,
    /// `MULTi`, `DUAL`, `Dual Audio`: includes more than one language.
    Multi,
}

impl Lang {
    /// ISO 639-1 code, or `multi`.
    pub fn code(self) -> &'static str {
        match self {
            Lang::English => "en",
            Lang::Italian => "it",
            Lang::French => "fr",
            Lang::German => "de",
            Lang::Spanish => "es",
            Lang::Russian => "ru",
            Lang::Hindi => "hi",
            Lang::Norwegian => "no",
            Lang::Multi => "multi",
        }
    }
}

/// Letters outside the Latin alphabet found in the text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scripts {
    pub cyrillic: bool,
    pub cjk: bool,
    /// Any other non-Latin letters (Arabic, Devanagari, Thai, Greek, ...).
    pub other: bool,
}

impl Scripts {
    pub fn any(self) -> bool {
        self.cyrillic || self.cjk || self.other
    }
}

/// Words that say the torrent is not a single film. Each is the word as it
/// was matched, so a caller can ignore the ones that are part of the film's
/// own title ("The Collection").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    pub pack_words: Vec<&'static str>,
    pub extras_words: Vec<&'static str>,
    /// A disc image or a full disc copy rather than a video file.
    pub disc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRelease {
    pub resolution: Resolution,
    pub source: Source,
    pub video: Video,
    pub bit10: bool,
    pub hdr: Hdr,
    pub dolby_vision: bool,
    /// Sorted and without duplicates.
    pub audio: Vec<Audio>,
    /// Every four-digit token from 1900 to 2099, in order, once each.
    pub years: Vec<u16>,
    /// Languages named as audio; a language followed by `subs` is left out.
    pub languages: Vec<Lang>,
    pub scripts: Scripts,
    pub flags: Flags,
    /// The text ends in `…` or `...`: Torrentio cut the name short, so tags
    /// at the end may be missing.
    pub truncated: bool,
}

impl ParsedRelease {
    /// Adds what `other` knows (the file name, the source's quality label) to
    /// what this one already has: unknown fields are filled in, tags are
    /// united, and a warning in either text stays.
    pub fn merge(&mut self, other: &ParsedRelease) {
        if self.resolution == Resolution::Unknown {
            self.resolution = other.resolution;
        }
        if self.source == Source::Unknown {
            self.source = other.source;
        }
        // A poor copy in either text is the more important fact.
        if other.source.is_poor_copy() && !self.source.is_poor_copy() {
            self.source = other.source;
        }
        if self.video == Video::Unknown {
            self.video = other.video;
        }
        self.bit10 |= other.bit10;
        self.hdr = self.hdr.max(other.hdr);
        self.dolby_vision |= other.dolby_vision;
        for audio in &other.audio {
            if !self.audio.contains(audio) {
                self.audio.push(*audio);
            }
        }
        self.audio.sort();
        for year in &other.years {
            if !self.years.contains(year) {
                self.years.push(*year);
            }
        }
        for lang in &other.languages {
            if !self.languages.contains(lang) {
                self.languages.push(*lang);
            }
        }
        self.scripts.cyrillic |= other.scripts.cyrillic;
        self.scripts.cjk |= other.scripts.cjk;
        self.scripts.other |= other.scripts.other;
        for word in &other.flags.pack_words {
            if !self.flags.pack_words.contains(word) {
                self.flags.pack_words.push(*word);
            }
        }
        for word in &other.flags.extras_words {
            if !self.flags.extras_words.contains(word) {
                self.flags.extras_words.push(*word);
            }
        }
        self.flags.disc |= other.flags.disc;
    }
}

// ---------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------

/// Audio names that are glued to a channel count: `aac5` is `aac`, `5`.
const CODEC_PREFIXES: &[&str] = &["aac", "ddp", "dd", "dts", "opus", "flac", "truehd", "atmos"];

/// Latin letters with accents, folded to plain ASCII.
fn fold(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ç' | 'ć' | 'č' => "c",
        'ď' | 'đ' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'ğ' => "g",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ı' => "i",
        'ł' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => "o",
        'ř' => "r",
        'ś' | 'š' | 'ş' => "s",
        'ť' | 'ţ' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        _ => return None,
    })
}

/// Lowercases, folds accents, drops apostrophes ("Schindler's" becomes
/// "schindlers"), spells `&` as "and", and splits on everything that is not a
/// letter or a digit.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut folded = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '\'' | '\u{2018}' | '\u{2019}' => {}
            '&' => folded.push_str(" and "),
            c => {
                for lower in c.to_lowercase() {
                    match fold(lower) {
                        Some(ascii) => folded.push_str(ascii),
                        None => folded.push(lower),
                    }
                }
            }
        }
    }
    // The plus sign is the only thing that tells these apart from their
    // plain forms, and splitting would lose it.
    let folded = folded.replace("dd+", "ddp").replace("hdr10+", "hdr10plus");

    let mut tokens = Vec::new();
    let mut current = String::new();
    for c in folded.chars() {
        if c.is_alphanumeric() {
            current.push(c);
        } else if !current.is_empty() {
            push_token(&mut tokens, std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        push_token(&mut tokens, current);
    }
    tokens
}

fn push_token(tokens: &mut Vec<String>, token: String) {
    let letters_end = token
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(token.len());
    let (head, tail) = token.split_at(letters_end);
    if !tail.is_empty()
        && tail.bytes().all(|b| b.is_ascii_digit())
        && CODEC_PREFIXES.contains(&head)
    {
        tokens.push(head.to_string());
        tokens.push(tail.to_string());
    } else {
        tokens.push(token);
    }
}

/// Four-digit tokens from 1900 to 2099.
pub fn years_in(tokens: &[String]) -> Vec<u16> {
    let mut years = Vec::new();
    for token in tokens {
        if token.len() == 4
            && let Ok(year) = token.parse::<u16>()
            && (1900..=2099).contains(&year)
            && !years.contains(&year)
        {
            years.push(year);
        }
    }
    years
}

struct Toks<'a>(&'a [String]);

impl Toks<'_> {
    fn has(&self, word: &str) -> bool {
        self.0.iter().any(|t| t == word)
    }

    fn has_any(&self, words: &[&str]) -> bool {
        words.iter().any(|w| self.has(w))
    }

    /// The tokens in a row, in this order.
    fn seq(&self, words: &[&str]) -> bool {
        self.0
            .windows(words.len())
            .any(|window| window.iter().zip(words).all(|(t, w)| t == w))
    }
}

// ---------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------

pub fn parse_release(text: &str) -> ParsedRelease {
    let tokens = tokenize(text);
    let t = Toks(&tokens);
    ParsedRelease {
        resolution: resolution(&t),
        source: source(&t, text),
        video: video(&t),
        bit10: t.has_any(&["10bit", "hi10", "hi10p"]) || t.seq(&["10", "bit"]),
        hdr: hdr(&t),
        dolby_vision: t.has_any(&["dv", "dovi", "dolbyvision"]) || t.seq(&["dolby", "vision"]),
        audio: audio(&t),
        years: years_in(&tokens),
        languages: languages(&tokens),
        scripts: scripts(text),
        flags: flags(&t),
        truncated: {
            let end = text.trim_end();
            end.ends_with('…') || end.ends_with("...")
        },
    }
}

fn resolution(t: &Toks) -> Resolution {
    // An explicit "1080p" beats the word "4K": a 1080p encode of a 4K
    // remaster is still 1080p.
    let explicit = [
        ("2160p", Resolution::R2160),
        ("1440p", Resolution::R1440),
        ("1080p", Resolution::R1080),
        ("1080i", Resolution::R1080),
        ("720p", Resolution::R720),
        ("576p", Resolution::Sd),
        ("480p", Resolution::Sd),
    ];
    let best = explicit
        .iter()
        .filter(|(word, _)| t.has(word))
        .map(|(_, r)| *r)
        .max();
    if let Some(best) = best {
        return best;
    }
    if t.has_any(&["4k", "uhd", "2160"]) {
        return Resolution::R2160;
    }
    if t.has_any(&["dvd", "dvdrip", "xvid", "divx"]) {
        return Resolution::Sd;
    }
    Resolution::Unknown
}

fn source(t: &Toks, raw: &str) -> Source {
    let web_rip = t.has("webrip") || t.seq(&["web", "rip"]);
    let blu = t.has_any(&["bluray", "bdrip", "brrip", "bd", "uhdrip"]) || t.seq(&["blu", "ray"]);
    let remux = t.has_any(&["remux", "bdremux"]);
    let web_dl = t.has("webdl") || t.seq(&["web", "dl"]) || (t.has("web") && !web_rip);
    let legit = remux || blu || web_rip || web_dl;

    // Copies that say what they are, wherever they appear.
    if t.has_any(&["hdcam", "camrip"]) {
        return Source::Cam;
    }
    if t.has_any(&["hdts", "telesync", "hdtc", "telecine", "pdvd", "predvd"]) {
        return Source::Telesync;
    }
    if t.has_any(&["dvdscr", "screener", "workprint"]) {
        return Source::Screener;
    }
    // Short words that are only believed when nothing says "BluRay" or
    // "WEB": a `.ts` file extension is not a telesync either.
    if !legit {
        let ts_is_extension = raw.trim_end().to_ascii_lowercase().ends_with(".ts");
        if t.has("cam") {
            return Source::Cam;
        }
        if (t.has("ts") && !ts_is_extension) || t.has("tc") {
            return Source::Telesync;
        }
        if t.has_any(&["scr", "r5", "wp"]) {
            return Source::Screener;
        }
    }

    if remux {
        Source::Remux
    } else if blu {
        Source::BluRay
    } else if web_dl {
        Source::WebDl
    } else if web_rip {
        Source::WebRip
    } else if t.has_any(&["hdtv", "pdtv", "tvrip"]) {
        Source::Hdtv
    } else if t.has("hdrip") {
        Source::Hdrip
    } else if t.has_any(&["dvd", "dvdrip", "dvd5", "dvd9"]) {
        Source::Dvd
    } else {
        Source::Unknown
    }
}

fn video(t: &Toks) -> Video {
    let tokens = t.0;
    for (i, token) in tokens.iter().enumerate() {
        let next = tokens.get(i + 1).map(String::as_str);
        let found = match (token.as_str(), next) {
            ("x264" | "h264" | "avc", _) => Video::H264,
            ("h", Some("264")) => Video::H264,
            ("x265" | "h265" | "hevc", _) => Video::H265,
            ("h", Some("265")) => Video::H265,
            ("av1", _) => Video::Av1,
            ("xvid", _) => Video::Xvid,
            ("divx", _) => Video::Divx,
            ("mpeg2", _) | ("mpeg", Some("2")) => Video::Mpeg2,
            ("vc1", _) | ("vc", Some("1")) => Video::Vc1,
            _ => continue,
        };
        return found;
    }
    Video::Unknown
}

fn hdr(t: &Toks) -> Hdr {
    if t.has("hdr10plus") || t.seq(&["hdr10", "plus"]) {
        Hdr::Hdr10Plus
    } else if t.has("hlg") {
        Hdr::Hlg
    } else if t.has_any(&["hdr", "hdr10"]) {
        Hdr::Hdr
    } else {
        Hdr::None
    }
}

fn audio(t: &Toks) -> Vec<Audio> {
    let mut found = Vec::new();
    let mut add = |audio: Audio| {
        if !found.contains(&audio) {
            found.push(audio);
        }
    };
    if t.has("aac") {
        add(Audio::Aac);
    }
    // "AC-3" is two tokens, and so is the end of "E-AC-3": tell them apart.
    let mut ac3 = t.has_any(&["ac3", "dd"])
        || (t.seq(&["dolby", "digital"]) && !t.seq(&["dolby", "digital", "plus"]));
    let mut eac3 = t.has_any(&["eac3", "ddp", "ddplus"]) || t.seq(&["dolby", "digital", "plus"]);
    for (i, pair) in t.0.windows(2).enumerate() {
        if pair[0] == "ac" && pair[1] == "3" {
            if i > 0 && t.0[i - 1] == "e" {
                eac3 = true;
            } else {
                ac3 = true;
            }
        }
    }
    if ac3 {
        add(Audio::Ac3);
    }
    if eac3 {
        add(Audio::Eac3);
    }
    if t.has_any(&["dts", "dtshd", "dtshdma", "dtsx"]) {
        add(Audio::Dts);
    }
    // "DTS-HD", "DTS-HD.MA", "DTS-HDMA" and the "DTS-DH" typo.
    if t.has_any(&["dtshd", "dtshdma"])
        || t.seq(&["dts", "hd"])
        || t.seq(&["dts", "hdma"])
        || t.seq(&["dts", "dh"])
    {
        add(Audio::DtsHd);
    }
    if t.has("truehd") || t.seq(&["true", "hd"]) {
        add(Audio::TrueHd);
    }
    if t.has("atmos") {
        add(Audio::Atmos);
    }
    if t.has("flac") {
        add(Audio::Flac);
    }
    if t.has_any(&["lpcm", "pcm"]) {
        add(Audio::Lpcm);
    }
    if t.has("opus") {
        add(Audio::Opus);
    }
    if t.has("mp3") {
        add(Audio::Mp3);
    }
    if t.has("vorbis") {
        add(Audio::Vorbis);
    }
    found.sort();
    found
}

fn language_of(token: &str) -> Option<Lang> {
    Some(match token {
        "eng" | "english" => Lang::English,
        "ita" | "italian" => Lang::Italian,
        "fre" | "french" | "truefrench" | "vff" => Lang::French,
        "ger" | "german" | "deu" => Lang::German,
        "spa" | "spanish" | "latino" | "castellano" => Lang::Spanish,
        "rus" | "russian" => Lang::Russian,
        "hindi" => Lang::Hindi,
        "nor" | "norwegian" | "norsk" => Lang::Norwegian,
        "multi" | "dual" => Lang::Multi,
        _ => return None,
    })
}

fn languages(tokens: &[String]) -> Vec<Lang> {
    const SUBTITLE_WORDS: &[&str] = &["sub", "subs", "subbed", "subtitles", "subtitled", "hardsub"];
    let mut found = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let Some(lang) = language_of(token) else {
            continue;
        };
        let is_subtitle = tokens
            .get(i + 1)
            .is_some_and(|next| SUBTITLE_WORDS.contains(&next.as_str()));
        if !is_subtitle && !found.contains(&lang) {
            found.push(lang);
        }
    }
    found
}

fn scripts(text: &str) -> Scripts {
    let mut scripts = Scripts::default();
    for c in text.chars() {
        let code = c as u32;
        match code {
            0x0400..=0x052F => scripts.cyrillic = true,
            0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF => {
                scripts.cjk = true;
            }
            // ASCII, Latin-1, Latin Extended and the Latin additional block.
            0..=0x024F | 0x1E00..=0x1EFF => {}
            _ if c.is_alphabetic() => scripts.other = true,
            _ => {}
        }
    }
    scripts
}

fn flags(t: &Toks) -> Flags {
    let mut flags = Flags::default();
    for word in [
        "pack",
        "collection",
        "trilogy",
        "quadrilogy",
        "anthology",
        "boxset",
    ] {
        if t.has(word) {
            flags.pack_words.push(word);
        }
    }
    if t.seq(&["complete", "series"]) {
        flags.pack_words.push("complete series");
    }
    if t.seq(&["top", "250"]) || t.seq(&["top", "100"]) || t.seq(&["imdb", "top"]) {
        flags.pack_words.push("top list");
    }
    for word in ["featurettes", "featurette", "extras", "sample", "trailer"] {
        if t.has(word) {
            flags.extras_words.push(word);
        }
    }
    flags.disc = t.has_any(&[
        "bdmv", "iso", "bd25", "bd50", "bd66", "bd100", "dvd5", "dvd9",
    ]) || t.seq(&["blu", "ray", "disc"])
        || t.seq(&["bluray", "disc"])
        || t.seq(&["complete", "bluray"])
        || t.seq(&["complete", "blu", "ray"])
        || t.seq(&["full", "disc"]);
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use Audio::*;

    struct Case {
        name: &'static str,
        res: Resolution,
        src: Source,
        video: Video,
        audio: &'static [Audio],
        dv: bool,
        hdr: Hdr,
    }

    const fn case(
        name: &'static str,
        res: Resolution,
        src: Source,
        video: Video,
        audio: &'static [Audio],
        dv: bool,
        hdr: Hdr,
    ) -> Case {
        Case {
            name,
            res,
            src,
            video,
            audio,
            dv,
            hdr,
        }
    }

    use Hdr as H;
    use Resolution as R;
    use Source as S;
    use Video as V;

    // Every name is invented: real tag patterns, made-up titles and groups.
    const CASES: &[Case] = &[
        // Audio forms.
        case(
            "Example.Movie.1994.1080p.BluRay.x265.10bit.AAC5.1-GRPONE",
            R::R1080,
            S::BluRay,
            V::H265,
            &[Aac],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.x265.AAC.5.1-GRPONE",
            R::R1080,
            S::BluRay,
            V::H265,
            &[Aac],
            false,
            H::None,
        ),
        case(
            "Example Movie (1994) (1080p BluRay x265 HEVC 10bit AAC 5.1 GrpTwo)",
            R::R1080,
            S::BluRay,
            V::H265,
            &[Aac],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.720p.WEB-DL.AAC2.0.H.264-GRPONE",
            R::R720,
            S::WebDl,
            V::H264,
            &[Aac],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.WEB-DL.DDP5.1.H264-GRPONE",
            R::R1080,
            S::WebDl,
            V::H264,
            &[Eac3],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 1080p WEB-DL DDP 5 1 H 264-GRPONE",
            R::R1080,
            S::WebDl,
            V::H264,
            &[Eac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.NF.WEBRip.DD+5.1.x264-GRPONE",
            R::R1080,
            S::WebRip,
            V::H264,
            &[Eac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.DD5.1.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Ac3],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 1080p BluRay DD 5.1 x265-GRPONE",
            R::R1080,
            S::BluRay,
            V::H265,
            &[Ac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.AC3.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Ac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.AC-3.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Ac3],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 1080p FR EN x264 ac3 GrpOne",
            R::R1080,
            S::Unknown,
            V::H264,
            &[Ac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.E-AC-3.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Eac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.EAC3.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Eac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.DDP5.1.Atmos.x265-GRPONE",
            R::R1080,
            S::BluRay,
            V::H265,
            &[Eac3, Atmos],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.2160p.UHD.BluRay.x265.10bit.HDR.DTS-HD.MA.5.1-GRPONE",
            R::R2160,
            S::BluRay,
            V::H265,
            &[Dts, DtsHd],
            false,
            H::Hdr,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.DTS-HDMA.5.1.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Dts, DtsHd],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.DTSHD-MA.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Dts, DtsHd],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.DTS.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Dts],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.2160p.BluRay.DTS-X.7.1.HEVC-GRPONE",
            R::R2160,
            S::BluRay,
            V::H265,
            &[Dts],
            false,
            H::None,
        ),
        case(
            "Example Movie (1994) 2160p H265 HDR10 DV ITA DTS-DH MA 5.1 ITA ENG AC3 5.1",
            R::R2160,
            S::Unknown,
            V::H265,
            &[Ac3, Dts, DtsHd],
            true,
            H::Hdr,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.TrueHD.7.1.Atmos.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[TrueHd, Atmos],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.FLAC.2.0.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Flac],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.LPCM.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Lpcm],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.bluray.hdr.2160p.av1.5.1.opus.subs-GRPONE",
            R::R2160,
            S::BluRay,
            V::Av1,
            &[Opus],
            false,
            H::Hdr,
        ),
        case(
            "Example Movie 1994 Bluray 1080p AV1 EN/FR/ES OPUS5.1-GRPONE",
            R::R1080,
            S::BluRay,
            V::Av1,
            &[Opus],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.MP3.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Mp3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.Vorbis.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Vorbis],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.AAC.AC3.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[Aac, Ac3],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 1080p Dolby Digital x264-GRPONE",
            R::R1080,
            S::Unknown,
            V::H264,
            &[Ac3],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 1080p Dolby Digital Plus x264-GRPONE",
            R::R1080,
            S::Unknown,
            V::H264,
            &[Eac3],
            false,
            H::None,
        ),
        // Releases that name no audio codec at all.
        case(
            "Example Movie (1994) 1080p BrRip x264 - GRPTWO",
            R::R1080,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.REMASTERED.1080p.10bit.BluRay.6CH.x265.HEVC-GRPTWO",
            R::R1080,
            S::BluRay,
            V::H265,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.BluRay.1080p.x265.10bit.5Audio.GRPTWO",
            R::R1080,
            S::BluRay,
            V::H265,
            &[],
            false,
            H::None,
        ),
        case(
            "Example Movie (1994) 1080p Surround",
            R::R1080,
            S::Unknown,
            V::Unknown,
            &[],
            false,
            H::None,
        ),
        // Sources.
        case(
            "Example.Movie.1994.2160p.BluRay.REMUX.HEVC.DTS-HD.MA.5.1-GRPONE",
            R::R2160,
            S::Remux,
            V::H265,
            &[Dts, DtsHd],
            false,
            H::None,
        ),
        case(
            "Example Movie (1994) BD REMUX (AVC DTSHD-MA) - GRPONE",
            R::Unknown,
            S::Remux,
            V::H264,
            &[Dts, DtsHd],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 UHD BDRemux 2160p HDR10",
            R::R2160,
            S::Remux,
            V::Unknown,
            &[],
            false,
            H::Hdr,
        ),
        case(
            "Example.Movie.1994.UHDRip.2160p.Hevc.HDR-GRPONE",
            R::R2160,
            S::BluRay,
            V::H265,
            &[],
            false,
            H::Hdr,
        ),
        case(
            "Example Movie [1994 BDRip-AVC]",
            R::Unknown,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example Movie [1994 HDRip]",
            R::Unknown,
            S::Hdrip,
            V::Unknown,
            &[],
            false,
            H::None,
        ),
        case(
            "Example Movie [1994 DVDRip] XviD",
            R::Sd,
            S::Dvd,
            V::Xvid,
            &[],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 DVDRip XviD AC3-GRPONE",
            R::Sd,
            S::Dvd,
            V::Xvid,
            &[Ac3],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.WEBRip.x264-GRPONE",
            R::R1080,
            S::WebRip,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.WEB.H264-GRPONE",
            R::R1080,
            S::WebDl,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.720p.HDTV.x264-GRPONE",
            R::R720,
            S::Hdtv,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.Blu-ray.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        // Resolution words.
        case(
            "Example Movie (1994) 4K Remastered 1080p 10bit Bluray x265 HEVC",
            R::R1080,
            S::BluRay,
            V::H265,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.4K.HDR.2160p.BDRemux.x265-GRPONE",
            R::R2160,
            S::Remux,
            V::H265,
            &[],
            false,
            H::Hdr,
        ),
        case(
            "Example Movie (1994) UHD BDRip 1440p HDR10",
            R::R1440,
            S::BluRay,
            V::Unknown,
            &[],
            false,
            H::Hdr,
        ),
        case(
            "Example Movie (1994) RM4K (1080p BluRay x265 GrpTwo)",
            R::R1080,
            S::BluRay,
            V::H265,
            &[],
            false,
            H::None,
        ),
        case(
            "Example Movie 1994 UHD BluRay HDR",
            R::R2160,
            S::BluRay,
            V::Unknown,
            &[],
            false,
            H::Hdr,
        ),
        // Dolby Vision and HDR.
        case(
            "Example.Movie.1994.2160p.WEB-DL.DV.HDR.H265-GRPONE",
            R::R2160,
            S::WebDl,
            V::H265,
            &[],
            true,
            H::Hdr,
        ),
        case(
            "Example Movie 1994 REPACK UHD BluRay 1080p DDP 5 1 DoVi HDR10 x265-GRPONE",
            R::R1080,
            S::BluRay,
            V::H265,
            &[Eac3],
            true,
            H::Hdr,
        ),
        case(
            "Example.Movie.1994.2160p.Dolby.Vision.WEB-DL.H265-GRPONE",
            R::R2160,
            S::WebDl,
            V::H265,
            &[],
            true,
            H::None,
        ),
        case(
            "Example.Movie.1994.2160p.WEB-DL.HDR10+.H265-GRPONE",
            R::R2160,
            S::WebDl,
            V::H265,
            &[],
            false,
            H::Hdr10Plus,
        ),
        case(
            "Example.Movie.1994.2160p.WEB-DL.HLG.H265-GRPONE",
            R::R2160,
            S::WebDl,
            V::H265,
            &[],
            false,
            H::Hlg,
        ),
        // Video codecs.
        case(
            "Example.Movie.1994.1080p.BluRay.H.264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.H.265-GRPONE",
            R::R1080,
            S::BluRay,
            V::H265,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.AVC-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.DivX-GRPONE",
            R::R1080,
            S::BluRay,
            V::Divx,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.MPEG-2-GRPONE",
            R::R1080,
            S::BluRay,
            V::Mpeg2,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.VC-1-GRPONE",
            R::R1080,
            S::BluRay,
            V::Vc1,
            &[],
            false,
            H::None,
        ),
        // Poor copies.
        case(
            "Example.Movie.1994.HDCAM.x264-GRPONE",
            R::Unknown,
            S::Cam,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example Movie (1994) CAMRip XviD",
            R::Sd,
            S::Cam,
            V::Xvid,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.HDTS.x264.AAC-GRPONE",
            R::Unknown,
            S::Telesync,
            V::H264,
            &[Aac],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.TELESYNC.XviD-GRPONE",
            R::Sd,
            S::Telesync,
            V::Xvid,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.HDTC.x264-GRPONE",
            R::Unknown,
            S::Telesync,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.TS.XviD-GRPONE",
            R::Sd,
            S::Telesync,
            V::Xvid,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.DVDSCR.XviD-GRPONE",
            R::Sd,
            S::Screener,
            V::Xvid,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.SCREENER.x264-GRPONE",
            R::Unknown,
            S::Screener,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.WORKPRINT.XviD-GRPONE",
            R::Sd,
            S::Screener,
            V::Xvid,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.R5.XviD-GRPONE",
            R::Sd,
            S::Screener,
            V::Xvid,
            &[],
            false,
            H::None,
        ),
        // Look-alikes that must not be read as anything.
        case(
            "Ghosts.1994.1080p.BluRay.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Add.Water.1994.1080p.BluRay.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.DVDRip.x264-GRPONE",
            R::Sd,
            S::Dvd,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.BluRay.TS.x264-GRPONE",
            R::R1080,
            S::BluRay,
            V::H264,
            &[],
            false,
            H::None,
        ),
        case(
            "Example.Movie.1994.1080p.ts",
            R::R1080,
            S::Unknown,
            V::Unknown,
            &[],
            false,
            H::None,
        ),
        // A series-style name must not upset the parser.
        case(
            "Example.Show.S01E01.1080p.WEB-DL.DDP5.1.H.264-GRPONE",
            R::R1080,
            S::WebDl,
            V::H264,
            &[Eac3],
            false,
            H::None,
        ),
    ];

    #[test]
    fn the_tag_table_reads_every_name_as_written() {
        assert!(CASES.len() >= 60, "the table should stay broad");
        for c in CASES {
            let parsed = parse_release(c.name);
            assert_eq!(parsed.resolution, c.res, "resolution of {}", c.name);
            assert_eq!(parsed.source, c.src, "source of {}", c.name);
            assert_eq!(parsed.video, c.video, "video of {}", c.name);
            assert_eq!(parsed.audio, c.audio, "audio of {}", c.name);
            assert_eq!(parsed.dolby_vision, c.dv, "dolby vision of {}", c.name);
            assert_eq!(parsed.hdr, c.hdr, "hdr of {}", c.name);
        }
    }

    #[test]
    fn a_ten_bit_flag_is_found_in_its_spellings() {
        for name in [
            "Example.Movie.1994.1080p.10bit.x265",
            "Example Movie 1994 1080p 10-bit x265",
            "Example Movie 1994 1080p Hi10P x264",
        ] {
            assert!(parse_release(name).bit10, "{name}");
        }
        assert!(!parse_release("Example.Movie.1994.1080p.x265").bit10);
    }

    #[test]
    fn look_alike_words_are_not_read_as_tags() {
        let ghosts = parse_release("Ghosts.2021.1080p.WEB.H264");
        assert_ne!(ghosts.source, Source::Telesync);
        // "DVDRip" is not Dolby Vision, "Add" is not DD.
        assert!(!parse_release("Example.Movie.1994.DVDRip.x264").dolby_vision);
        assert!(
            !parse_release("Add.Water.1994.1080p")
                .audio
                .contains(&Audio::Ac3)
        );
        // The "5.1" after AV1 is a channel count, and opus is not AAC.
        let av1 = parse_release("Example.Movie.1994.bluray.hdr.2160p.av1.5.1.opus.subs-GRPONE");
        assert_eq!(av1.audio, [Audio::Opus]);
        assert!(!av1.audio.contains(&Audio::Aac));
        // Both audio tracks of this name are found.
        let typo = parse_release(
            "Example Movie (1994) 2160p H265 HDR10 DV ITA DTS-DH MA 5.1 ITA ENG AC3 5.1",
        );
        assert!(typo.audio.contains(&Audio::Dts) && typo.audio.contains(&Audio::Ac3));
    }

    #[test]
    fn a_file_extension_is_not_a_telesync() {
        let parsed = parse_release("Example.Movie.1994.1080p.ts");
        assert_eq!(parsed.source, Source::Unknown);
        // But a real TS tag is, even with other tags around it.
        assert_eq!(
            parse_release("Example.Movie.1994.TS.XviD").source,
            Source::Telesync
        );
        // And BluRay vouches for a stray "ts" token.
        assert_eq!(
            parse_release("Example.Movie.1994.1080p.BluRay.TS.x264").source,
            Source::BluRay
        );
    }

    #[test]
    fn years_are_four_digit_tokens_between_1900_and_2099() {
        assert_eq!(parse_release("1917.2019.1080p.BluRay").years, [1917, 2019]);
        assert_eq!(
            parse_release("Blade.Runner.2049.2017.1080p").years,
            [2049, 2017]
        );
        assert_eq!(
            parse_release("Example Movie [1994 BDRip 1080p]").years,
            [1994]
        );
        // No year inside a resolution or a codec.
        assert!(parse_release("Example.Movie.2160p.x264").years.is_empty());
        // Once each, in order.
        assert_eq!(parse_release("2001.2001.1968").years, [2001, 1968]);
    }

    #[test]
    fn languages_are_audio_and_a_subtitle_language_is_not() {
        let lang = |name: &str| parse_release(name).languages;
        assert_eq!(lang("Example.Movie.1994.1080p.ENG.SUBS"), []);
        assert_eq!(lang("Example.Movie.1994.AC3.ITA.ENG.SUBS"), [Lang::Italian]);
        assert_eq!(
            lang("Example.Movie.1994.Rus.Eng.BDRip"),
            [Lang::Russian, Lang::English]
        );
        assert_eq!(lang("Example.Movie.1994.MULTi.1080p"), [Lang::Multi]);
        assert_eq!(lang("Example.Movie.1994.DUAL.5.1"), [Lang::Multi]);
        assert_eq!(lang("Example Movie 1994 Dual-Audio"), [Lang::Multi]);
        assert_eq!(lang("Example.Movie.1994.MULTi.SUBS"), []);
        assert_eq!(lang("Example.Movie.1994.TRUEFRENCH.720p"), [Lang::French]);
        assert_eq!(
            lang("Example.Movie.1994.1080p.NORWEGIAN"),
            [Lang::Norwegian]
        );
        assert_eq!(lang("Example.Movie.1994.1080p.x264"), []);
        assert_eq!(Lang::English.code(), "en");
    }

    #[test]
    fn scripts_are_noticed() {
        let cyr = parse_release("Пример Фильма / Example Movie [1994 BDRip 1080p]");
        assert!(cyr.scripts.cyrillic && !cyr.scripts.cjk && !cyr.scripts.other);
        assert_eq!(cyr.years, [1994]);
        assert_eq!(cyr.source, Source::BluRay);
        assert!(
            parse_release("例子电影 Example Movie 1994 1080p")
                .scripts
                .cjk
        );
        assert!(parse_release("Ελληνική Ταινία 1994").scripts.other);
        let latin = parse_release("Amélie.2001.1080p.BluRay");
        assert!(!latin.scripts.any());
    }

    #[test]
    fn pack_extras_and_disc_words_are_collected() {
        let p = parse_release("Example Writer Pack PACK (1985 - 2003) [GRPONE]");
        assert_eq!(p.flags.pack_words, ["pack"]);
        assert_eq!(
            parse_release("Example Pictures Collection (1985 - 2003)")
                .flags
                .pack_words,
            ["collection"]
        );
        assert_eq!(
            parse_release("Example IMDB Top List 01-50 BluRay 1080p")
                .flags
                .pack_words,
            ["top list"]
        );
        assert_eq!(
            parse_release("Example.Movie.Trilogy.1080p")
                .flags
                .pack_words,
            ["trilogy"]
        );
        assert_eq!(
            parse_release("Example Movie (1994) Featurettes (2160p BluRay)")
                .flags
                .extras_words,
            ["featurettes"]
        );
        assert_eq!(
            parse_release("Example Movie 1994 1080p Sample")
                .flags
                .extras_words,
            ["sample"]
        );
        for disc in [
            "Example Movie [1994 Blu-ray disc 1080p]",
            "Example.Movie.1994.COMPLETE.BLURAY-GRPONE",
            "Example.Movie.1994.BDMV.1080p",
            "Example.Movie.1994.ISO",
            "Example.Movie.1994.BD50",
        ] {
            assert!(parse_release(disc).flags.disc, "{disc}");
        }
        let plain = parse_release("Example.Movie.1994.1080p.BluRay.x264-GRPONE");
        assert_eq!(plain.flags, Flags::default());
    }

    #[test]
    fn a_name_cut_short_is_marked_truncated() {
        assert!(
            parse_release("Example.Movie.1994.UHD.BluRay.2160p.DTS-HD.MA.5.1.DV.HEVC.HYBRID.REMU…")
                .truncated
        );
        assert!(
            parse_release("Example.Movie.1994.bluray.sdr.1080p.av1.5.1.opus.subs...").truncated
        );
        assert!(!parse_release("Example.Movie.1994.1080p.BluRay.x264-GRPONE").truncated);
        // A double dot inside a name is not a cut.
        assert!(!parse_release("Example Movie 1994 - 1080p FR EN x264 ac3 GrpOne..mkv").truncated);
    }

    #[test]
    fn tokens_ignore_case_accents_and_apostrophes() {
        assert_eq!(tokenize("Schindler's.List"), tokenize("SCHINDLERS LIST"));
        assert_eq!(tokenize("Amélie"), ["amelie"]);
        assert_eq!(tokenize("Fast & Furious"), ["fast", "and", "furious"]);
        assert_eq!(tokenize("WALL·E"), ["wall", "e"]);
        assert_eq!(tokenize("AAC5.1"), ["aac", "5", "1"]);
        assert_eq!(tokenize("DD+5.1"), ["ddp", "5", "1"]);
        // "ac3" and "x264" are single tokens, not split.
        assert_eq!(tokenize("AC3 x264"), ["ac3", "x264"]);
    }

    #[test]
    fn merging_the_file_name_adds_what_the_title_lacks() {
        let mut title = parse_release("Example.Movie.1994.UHD.BluRay.2160p.HEVC.HYBRID.REMU…");
        let file = parse_release(
            "Example.Movie.1994.UHD.BluRay.2160p.DTS-HD.MA.5.1.DV.HEVC.HYBRID.REMUX.mkv",
        );
        assert!(title.audio.is_empty() && !title.dolby_vision);
        title.merge(&file);
        assert!(title.audio.contains(&Audio::Dts) && title.dolby_vision);
        assert!(title.truncated, "the name itself stays marked as cut");
    }

    #[test]
    fn a_poor_copy_in_either_text_wins_the_merge() {
        let mut name = parse_release("Example.Movie.1994.1080p.x264");
        name.merge(&parse_release("Example.Movie.1994.HDTS.x264.mkv"));
        assert_eq!(name.source, Source::Telesync);
        let mut label = parse_release("Example.Movie.1994.1080p.x264");
        label.merge(&parse_release("4k DV | HDR"));
        assert!(label.dolby_vision);
        assert_eq!(label.hdr, Hdr::Hdr);
        assert_eq!(
            label.resolution,
            Resolution::R1080,
            "the name's own value stays"
        );
    }

    #[test]
    fn plain_strings_do_not_panic() {
        for text in [
            "",
            " ",
            "...",
            "…",
            "+",
            "dd+",
            "12345",
            "日本語",
            "\u{0}\u{7}",
        ] {
            let _ = parse_release(text);
        }
    }
}
