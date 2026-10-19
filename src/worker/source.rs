//! Where candidates come from. The worker is generic over this trait, so
//! the run loop is tested with a stand-in and a second source (Prowlarr, a
//! cache of its own) can be added without touching it.

use thiserror::Error;

use super::candidate::RawCandidate;

/// What is being looked for. Series are out of scope for now; the enum stays
/// closed so a new kind has to be handled everywhere on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Movie,
}

/// Why a source gave no answer. The two kinds are told apart because the
/// worker reports them differently: an unreachable source is worth trying
/// again, a source that answers in an unknown shape needs a person.
///
/// An empty `Ok(vec![])` is not an error: it means the source answered and
/// has nothing. An unreachable source is never turned into an empty list.
/// The strings are short and carry no body text.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SourceError {
    /// Network error, timeout, connect failure, 5xx or 429.
    #[error("source unreachable: {0}")]
    Unreachable(String),
    /// Not JSON, wrong shape, too big, a redirect, another 4xx, or every
    /// item invalid.
    #[error("bad response from source: {0}")]
    BadResponse(String),
}

// Callers are generic over `S: CandidateSource`, never `dyn`, so the
// auto-trait bounds the lint worries about are the caller's to name.
#[allow(async_fn_in_trait)]
pub trait CandidateSource {
    async fn candidates(
        &self,
        imdb_id: &str,
        kind: MediaKind,
    ) -> Result<Vec<RawCandidate>, SourceError>;
}
