//! What every candidate source hands the worker, whatever the source is.

/// One torrent a source offers for a title, before any judging. The source
/// has already checked the fields: the infohash is 40 lowercase hex
/// characters and the strings are free of control characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCandidate {
    pub infohash: String,
    /// The release name.
    pub name: String,
    /// The video file inside the torrent, when the source knows it.
    pub filename: Option<String>,
    /// The source listed this as a pack or collection holding other titles.
    pub pack: bool,
    /// The source's own quality label, such as `4k DV | HDR`.
    pub label: Option<String>,
    pub size_bytes: Option<u64>,
    pub seeders: Option<u32>,
    pub provider: Option<String>,
    /// ISO 639-1 codes of the languages the source flagged, lowercase.
    pub flag_languages: Vec<String>,
    /// The source says the torrent has several audio languages.
    pub multi_audio: bool,
}
