//! The worker process's own settings, read from the environment.
//!
//! This is deliberately not `crate::config::Config`: the worker holds no
//! portal secrets (no pepper, no TMDB token, no database) and must start
//! without them.

use std::fmt;

use thiserror::Error;

use super::settings::RankSettings;

const DEFAULT_PORTAL_URL: &str = "http://portal:8001";
const DEFAULT_TORRENTIO_URL: &str = "https://torrentio.strem.fun";
const DEFAULT_POLL_SECS: u64 = 10;
const MIN_POLL_SECS: u64 = 2;
const MAX_POLL_SECS: u64 = 300;

/// The same floor the portal applies to its side of the token.
const MIN_INTERNAL_TOKEN_BYTES: usize = 32;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[derive(Debug, Error)]
pub enum WorkerConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid {name}: {reason}")]
    Invalid { name: &'static str, reason: String },
}

fn invalid(name: &'static str, reason: impl Into<String>) -> WorkerConfigError {
    WorkerConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

pub struct WorkerConfig {
    pub portal_url: String,
    pub internal_token: String,
    pub poll_secs: u64,
    pub torrentio_base_url: String,
    pub rank: RankSettings,
}

impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("portal_url", &self.portal_url)
            .field("internal_token", &"<redacted>")
            .field("poll_secs", &self.poll_secs)
            .field("torrentio_base_url", &self.torrentio_base_url)
            .field("rank", &self.rank)
            .finish()
    }
}

impl WorkerConfig {
    pub fn from_env() -> Result<Self, WorkerConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, WorkerConfigError> {
        // A value that is unset, empty or blank counts as not given.
        let value = |name: &str| {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        // A Real-Debrid token in the plain environment is never the plan;
        // the file-based one belongs to a later step and is ignored here.
        if get("RD_TOKEN").is_some() {
            return Err(invalid(
                "RD_TOKEN",
                "the worker takes no Real-Debrid token in the environment",
            ));
        }

        let portal_url = http_url(
            "PORTAL_URL",
            value("PORTAL_URL").unwrap_or_else(|| DEFAULT_PORTAL_URL.to_string()),
        )?;
        let torrentio_base_url = http_url(
            "TORRENTIO_BASE_URL",
            value("TORRENTIO_BASE_URL").unwrap_or_else(|| DEFAULT_TORRENTIO_URL.to_string()),
        )?;

        let internal_token =
            value("INTERNAL_TOKEN").ok_or(WorkerConfigError::Missing("INTERNAL_TOKEN"))?;
        if internal_token.len() < MIN_INTERNAL_TOKEN_BYTES {
            return Err(invalid(
                "INTERNAL_TOKEN",
                format!("must be at least {MIN_INTERNAL_TOKEN_BYTES} bytes"),
            ));
        }

        let poll_secs = match value("WORKER_POLL_SECS") {
            None => DEFAULT_POLL_SECS,
            Some(v) => {
                let secs: u64 = v
                    .parse()
                    .map_err(|_| invalid("WORKER_POLL_SECS", "must be a whole number"))?;
                if !(MIN_POLL_SECS..=MAX_POLL_SECS).contains(&secs) {
                    return Err(invalid(
                        "WORKER_POLL_SECS",
                        format!("must be between {MIN_POLL_SECS} and {MAX_POLL_SECS}"),
                    ));
                }
                secs
            }
        };

        let mut rank = RankSettings::default();
        if let Some(v) = value("WORKER_ALLOWED_LANGUAGES") {
            let languages: Vec<String> = v
                .split(',')
                .map(|code| code.trim().to_ascii_lowercase())
                .filter(|code| !code.is_empty())
                .collect();
            if languages.is_empty() {
                return Err(invalid("WORKER_ALLOWED_LANGUAGES", "names no language"));
            }
            rank.allowed_languages = languages;
        }
        if let Some(v) = value("WORKER_MIN_SIZE_MB") {
            rank.min_size_bytes = whole_number("WORKER_MIN_SIZE_MB", &v)?.saturating_mul(MIB);
        }
        if let Some(v) = value("WORKER_MAX_SIZE_GB") {
            rank.max_size_bytes = whole_number("WORKER_MAX_SIZE_GB", &v)?.saturating_mul(GIB);
        }
        if rank.min_size_bytes > rank.max_size_bytes {
            return Err(invalid(
                "WORKER_MIN_SIZE_MB",
                "is larger than WORKER_MAX_SIZE_GB",
            ));
        }
        if let Some(v) = value("WORKER_CLEAR_MIN_SCORE") {
            rank.clear_min_score = points("WORKER_CLEAR_MIN_SCORE", &v)?;
        }
        if let Some(v) = value("WORKER_CLEAR_MARGIN") {
            rank.clear_margin = points("WORKER_CLEAR_MARGIN", &v)?;
        }

        Ok(Self {
            portal_url,
            internal_token,
            poll_secs,
            torrentio_base_url,
            rank,
        })
    }
}

/// An `http://` or `https://` base URL, without a trailing slash.
fn http_url(name: &'static str, url: String) -> Result<String, WorkerConfigError> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(invalid(name, "must start with http:// or https://"));
    }
    Ok(url.trim_end_matches('/').to_string())
}

fn whole_number(name: &'static str, text: &str) -> Result<u64, WorkerConfigError> {
    text.parse()
        .map_err(|_| invalid(name, "must be a whole number"))
}

fn points(name: &'static str, text: &str) -> Result<f64, WorkerConfigError> {
    match text.parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 => Ok(n),
        _ => Err(invalid(name, "must be a number of at least 0")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn load(pairs: &[(&str, &str)]) -> Result<WorkerConfig, WorkerConfigError> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        WorkerConfig::from_lookup(|name| env.get(name).cloned())
    }

    #[test]
    fn it_loads_with_none_of_the_portals_variables() {
        // No CODE_PEPPER, TMDB_TOKEN, PUBLIC_ORIGIN or ADMIN_* here.
        let config = load(&[("INTERNAL_TOKEN", TOKEN)]).unwrap();
        assert_eq!(config.internal_token, TOKEN);
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let config = load(&[("INTERNAL_TOKEN", TOKEN)]).unwrap();
        assert_eq!(config.portal_url, "http://portal:8001");
        assert_eq!(config.torrentio_base_url, "https://torrentio.strem.fun");
        assert_eq!(config.poll_secs, 10);
        assert_eq!(config.rank, RankSettings::default());
    }

    #[test]
    fn the_token_is_required_and_at_least_32_bytes() {
        assert!(matches!(
            load(&[]),
            Err(WorkerConfigError::Missing("INTERNAL_TOKEN"))
        ));
        assert!(matches!(
            load(&[("INTERNAL_TOKEN", "   ")]),
            Err(WorkerConfigError::Missing("INTERNAL_TOKEN"))
        ));
        let short = &TOKEN[..31];
        assert!(matches!(
            load(&[("INTERNAL_TOKEN", short)]),
            Err(WorkerConfigError::Invalid {
                name: "INTERNAL_TOKEN",
                ..
            })
        ));
        let padded = format!("  {TOKEN}\n");
        assert_eq!(
            load(&[("INTERNAL_TOKEN", &padded)]).unwrap().internal_token,
            TOKEN
        );
    }

    #[test]
    fn urls_need_a_scheme_and_lose_a_trailing_slash() {
        let config = load(&[
            ("INTERNAL_TOKEN", TOKEN),
            ("PORTAL_URL", "http://127.0.0.1:8001/"),
            ("TORRENTIO_BASE_URL", "https://example.invalid//"),
        ])
        .unwrap();
        assert_eq!(config.portal_url, "http://127.0.0.1:8001");
        assert_eq!(config.torrentio_base_url, "https://example.invalid");

        for (name, bad) in [
            ("PORTAL_URL", "portal:8001"),
            ("PORTAL_URL", "ftp://portal"),
            ("TORRENTIO_BASE_URL", "torrentio.example.invalid"),
        ] {
            assert!(
                matches!(
                    load(&[("INTERNAL_TOKEN", TOKEN), (name, bad)]),
                    Err(WorkerConfigError::Invalid { name: n, .. }) if n == name
                ),
                "{name}={bad}"
            );
        }
    }

    #[test]
    fn the_poll_interval_stays_in_range() {
        for (given, secs) in [("2", 2), ("45", 45), ("300", 300)] {
            let config = load(&[("INTERNAL_TOKEN", TOKEN), ("WORKER_POLL_SECS", given)]).unwrap();
            assert_eq!(config.poll_secs, secs);
        }
        for bad in ["1", "301", "0", "-5", "ten", "2.5"] {
            assert!(
                load(&[("INTERNAL_TOKEN", TOKEN), ("WORKER_POLL_SECS", bad)]).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_real_debrid_token_in_the_environment_is_refused() {
        assert!(matches!(
            load(&[("INTERNAL_TOKEN", TOKEN), ("RD_TOKEN", "anything")]),
            Err(WorkerConfigError::Invalid {
                name: "RD_TOKEN",
                ..
            })
        ));
        // Set but empty still counts as set.
        assert!(load(&[("INTERNAL_TOKEN", TOKEN), ("RD_TOKEN", "")]).is_err());
    }

    #[test]
    fn the_thresholds_can_be_overridden() {
        let config = load(&[
            ("INTERNAL_TOKEN", TOKEN),
            ("WORKER_ALLOWED_LANGUAGES", "EN, no,,sv"),
            ("WORKER_MIN_SIZE_MB", "700"),
            ("WORKER_MAX_SIZE_GB", "20"),
            ("WORKER_CLEAR_MIN_SCORE", "65.5"),
            ("WORKER_CLEAR_MARGIN", "10"),
        ])
        .unwrap();
        assert_eq!(config.rank.allowed_languages, ["en", "no", "sv"]);
        assert_eq!(config.rank.min_size_bytes, 700 * MIB);
        assert_eq!(config.rank.max_size_bytes, 20 * GIB);
        assert_eq!(config.rank.clear_min_score, 65.5);
        assert_eq!(config.rank.clear_margin, 10.0);
        // Everything else keeps its default.
        let default = RankSettings::default();
        assert_eq!(config.rank.resolution, default.resolution);
        assert_eq!(config.rank.shortlist_len, default.shortlist_len);
    }

    #[test]
    fn bad_threshold_overrides_are_refused() {
        for (name, bad) in [
            ("WORKER_ALLOWED_LANGUAGES", " , "),
            ("WORKER_MIN_SIZE_MB", "big"),
            ("WORKER_MAX_SIZE_GB", "-1"),
            ("WORKER_CLEAR_MIN_SCORE", "NaN"),
            ("WORKER_CLEAR_MIN_SCORE", "inf"),
            ("WORKER_CLEAR_MARGIN", "-3"),
        ] {
            assert!(
                load(&[("INTERNAL_TOKEN", TOKEN), (name, bad)]).is_err(),
                "{name}={bad}"
            );
        }
        // A minimum above the maximum can never keep anything.
        assert!(
            load(&[
                ("INTERNAL_TOKEN", TOKEN),
                ("WORKER_MIN_SIZE_MB", "50000"),
                ("WORKER_MAX_SIZE_GB", "1"),
            ])
            .is_err()
        );
    }

    #[test]
    fn debug_does_not_show_the_token() {
        let config = load(&[("INTERNAL_TOKEN", TOKEN)]).unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains(TOKEN));
        assert!(shown.contains("<redacted>"));
    }
}
