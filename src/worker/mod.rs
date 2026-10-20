//! The fetch worker: reading release names, filtering and ranking
//! candidates, and the clients that fetch them. Only `source` and
//! `torrentio` talk to a candidate source and only `portal` talks to the
//! portal; every rule elsewhere can be tested with plain strings.

pub mod backoff;
pub mod candidate;
pub mod config;
pub mod fetch;
pub mod files;
pub mod portal;
pub mod rd;
pub mod release;
pub mod rules;
pub mod run;
pub mod score;
pub mod settings;
pub mod source;
pub mod state;
pub mod torrentio;
pub mod torrentio_text;

pub use score::{Candidate, Evaluation, evaluate};
