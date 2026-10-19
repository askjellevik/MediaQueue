//! The numbers the worker judges candidates by. One struct with a
//! `Default`, so tuning from the dry-run data means changing values here
//! (or, for a few, from the environment in the runtime).

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[derive(Debug, Clone, PartialEq)]
pub struct RankSettings {
    /// A torrent smaller than this is dropped.
    pub min_size_bytes: u64,
    /// A torrent larger than this is dropped.
    pub max_size_bytes: u64,
    /// Languages that count as the wanted audio (ISO 639-1). A release whose
    /// language signals are all outside this list is penalised, not dropped.
    pub allowed_languages: Vec<String>,
}

impl Default for RankSettings {
    fn default() -> Self {
        Self {
            min_size_bytes: 400 * MIB,
            max_size_bytes: 40 * GIB,
            allowed_languages: vec!["en".to_string()],
        }
    }
}
