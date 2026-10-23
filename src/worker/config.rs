//! The worker process's own settings, read from the environment.
//!
//! This is deliberately not `crate::config::Config`: the worker holds no
//! portal secrets (no pepper, no TMDB token, no database) and must start
//! without them.

use std::{fmt, io::Read, os::unix::fs::MetadataExt, path::PathBuf};

use reqwest::Url;
use thiserror::Error;

use super::{
    rd::{DEFAULT_BASE_URL, Secret},
    settings::RankSettings,
};

const DEFAULT_PORTAL_URL: &str = "http://portal:8001";
const DEFAULT_TORRENTIO_URL: &str = "https://torrentio.strem.fun";
const DEFAULT_POLL_SECS: u64 = 10;
const MIN_POLL_SECS: u64 = 2;
const MAX_POLL_SECS: u64 = 300;

/// The same floor the portal applies to its side of the token.
const MIN_INTERNAL_TOKEN_BYTES: usize = 32;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

const MAX_AUTO_TRIES: u64 = 5;
const MAX_AUTO_GB: u64 = 100;

const DEFAULT_STATE_DIR: &str = "/state";
const DEFAULT_MAX_ADDS_PER_DAY: u32 = 5;
const DEFAULT_MAX_UNCACHED_PER_DAY: u32 = 2;
const DEFAULT_MAX_REFUSALS_PER_DAY: u32 = 20;
const DEFAULT_RD_MAX_GB: u64 = 30;
const DEFAULT_RD_PER_MIN: u32 = 20;
const DEFAULT_KEEP_FREE_SLOTS: u32 = 2;
/// Ceilings for the settings above: raising a cap further needs a code
/// change, not just an edit of `.env`.
const MAX_ADDS_PER_DAY_LIMIT: u64 = 50;
const MAX_UNCACHED_PER_DAY_LIMIT: u64 = 20;
const MAX_REFUSALS_PER_DAY_LIMIT: u64 = 200;
const MAX_RD_MAX_GB: u64 = 100;
const MAX_RD_PER_MIN: u64 = 30;
const MAX_KEEP_FREE_SLOTS: u64 = 20;
const MIN_TOKEN_CHARS: usize = 20;
const MAX_TOKEN_CHARS: usize = 128;
/// A token file is a few dozen bytes; anything bigger is not one.
const MAX_TOKEN_FILE_BYTES: u64 = 1024;

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

/// What the Real-Debrid modes need. Present only when `RD_TOKEN_FILE` is
/// set; without it the worker behaves as the dry-run worker always did.
#[derive(Debug)]
pub struct RdConfig {
    /// Redacted in `Debug`.
    pub token: Secret,
    pub base_url: String,
    pub state_dir: PathBuf,
    pub max_adds_per_day: u32,
    pub max_uncached_per_day: u32,
    /// Refusals by Real-Debrid after which the worker stops choosing
    /// torrents itself for the day. A human's pick is never stopped by it.
    pub max_refusals_per_day: u32,
    /// The cap on one torrent's file, checked against Real-Debrid's size.
    pub max_bytes: u64,
    pub per_min: u32,
    /// Active slots left unused for the admin's own downloads.
    pub keep_free_slots: u32,
}

pub struct WorkerConfig {
    pub portal_url: String,
    pub internal_token: String,
    pub poll_secs: u64,
    pub torrentio_base_url: String,
    pub rank: RankSettings,
    pub rd: Option<RdConfig>,
}

impl fmt::Debug for WorkerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkerConfig")
            .field("portal_url", &self.portal_url)
            .field("internal_token", &"<redacted>")
            .field("poll_secs", &self.poll_secs)
            .field("torrentio_base_url", &self.torrentio_base_url)
            .field("rank", &self.rank)
            .field("rd", &self.rd)
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

        // A Real-Debrid token in the plain environment is never the plan; it
        // comes from a file (`RD_TOKEN_FILE`).
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
        if let Some(v) = value("WORKER_PRO_GROUPS") {
            // Empty switches the bonus off.
            rank.pro_groups = v
                .split(',')
                .map(|group| group.trim().to_ascii_lowercase())
                .filter(|group| !group.is_empty())
                .collect();
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
        if let Some(v) = value("WORKER_AUTO_MIN_SCORE") {
            rank.auto_min_score = points("WORKER_AUTO_MIN_SCORE", &v)?;
        }
        if let Some(v) = value("WORKER_AUTO_MIN_SEEDERS") {
            rank.auto_min_seeders = u32::try_from(whole_number("WORKER_AUTO_MIN_SEEDERS", &v)?)
                .map_err(|_| invalid("WORKER_AUTO_MIN_SEEDERS", "is too large"))?;
        }
        if let Some(v) = value("WORKER_AUTO_MAX_TRIES") {
            let tries = whole_number("WORKER_AUTO_MAX_TRIES", &v)?;
            if !(1..=MAX_AUTO_TRIES).contains(&tries) {
                return Err(invalid(
                    "WORKER_AUTO_MAX_TRIES",
                    format!("must be between 1 and {MAX_AUTO_TRIES}"),
                ));
            }
            rank.auto_max_tries = tries as usize;
        }
        if let Some(v) = value("WORKER_AUTO_MAX_GB") {
            let gb = whole_number("WORKER_AUTO_MAX_GB", &v)?;
            if !(1..=MAX_AUTO_GB).contains(&gb) {
                return Err(invalid(
                    "WORKER_AUTO_MAX_GB",
                    format!("must be between 1 and {MAX_AUTO_GB}"),
                ));
            }
            rank.auto_max_bytes = gb * GIB;
        }

        let rd = match value("RD_TOKEN_FILE") {
            None => None,
            Some(file) => Some(rd_config(&value, &file)?),
        };

        Ok(Self {
            portal_url,
            internal_token,
            poll_secs,
            torrentio_base_url,
            rank,
            rd,
        })
    }
}

fn rd_config(
    value: &impl Fn(&str) -> Option<String>,
    token_file: &str,
) -> Result<RdConfig, WorkerConfigError> {
    let token = read_token_file(token_file)?;
    let base_url =
        rd_base_url(value("RD_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()))?;
    let state_dir =
        PathBuf::from(value("WORKER_STATE_DIR").unwrap_or_else(|| DEFAULT_STATE_DIR.to_string()));
    let max_adds_per_day = bounded(
        value,
        "WORKER_MAX_ADDS_PER_DAY",
        0,
        MAX_ADDS_PER_DAY_LIMIT,
        u64::from(DEFAULT_MAX_ADDS_PER_DAY),
    )?;
    let max_uncached_per_day = bounded(
        value,
        "WORKER_MAX_UNCACHED_PER_DAY",
        0,
        MAX_UNCACHED_PER_DAY_LIMIT,
        u64::from(DEFAULT_MAX_UNCACHED_PER_DAY),
    )?;
    let max_refusals_per_day = bounded(
        value,
        "WORKER_MAX_REFUSALS_PER_DAY",
        0,
        MAX_REFUSALS_PER_DAY_LIMIT,
        u64::from(DEFAULT_MAX_REFUSALS_PER_DAY),
    )?;
    let max_gb = bounded(
        value,
        "WORKER_RD_MAX_GB",
        1,
        MAX_RD_MAX_GB,
        DEFAULT_RD_MAX_GB,
    )?;
    let per_min = bounded(
        value,
        "WORKER_RD_PER_MIN",
        1,
        MAX_RD_PER_MIN,
        u64::from(DEFAULT_RD_PER_MIN),
    )?;
    let keep_free_slots = bounded(
        value,
        "WORKER_KEEP_FREE_SLOTS",
        0,
        MAX_KEEP_FREE_SLOTS,
        u64::from(DEFAULT_KEEP_FREE_SLOTS),
    )?;
    Ok(RdConfig {
        token,
        base_url,
        state_dir,
        max_adds_per_day: max_adds_per_day as u32,
        max_uncached_per_day: max_uncached_per_day as u32,
        max_refusals_per_day: max_refusals_per_day as u32,
        max_bytes: max_gb * GIB,
        per_min: per_min as u32,
        keep_free_slots: keep_free_slots as u32,
    })
}

/// A whole number in `min..=max`, or `default` when the variable is unset.
fn bounded(
    value: &impl Fn(&str) -> Option<String>,
    name: &'static str,
    min: u64,
    max: u64,
    default: u64,
) -> Result<u64, WorkerConfigError> {
    let Some(text) = value(name) else {
        return Ok(default);
    };
    let n = whole_number(name, &text)?;
    if !(min..=max).contains(&n) {
        return Err(invalid(name, format!("must be between {min} and {max}")));
    }
    Ok(n)
}

/// The base URL of the Real-Debrid API: `https://`, or plain http only to
/// the machine itself (for tests), and no credentials in it.
fn rd_base_url(text: String) -> Result<String, WorkerConfigError> {
    let bad = || {
        invalid(
            "RD_BASE_URL",
            "must be https://, or http://127.0.0.1 for tests",
        )
    };
    let url = Url::parse(&text).map_err(|_| bad())?;
    let local = url.scheme() == "http" && url.host_str() == Some("127.0.0.1");
    if !(url.scheme() == "https" || local) || !url.username().is_empty() || url.password().is_some()
    {
        return Err(bad());
    }
    Ok(text.trim_end_matches('/').to_string())
}

/// Reads the token once: a small file nobody else can read, holding 20 to
/// 128 printable ASCII characters. The reasons never quote the file.
fn read_token_file(path: &str) -> Result<Secret, WorkerConfigError> {
    let bad = |reason: &str| invalid("RD_TOKEN_FILE", reason);
    let file = std::fs::File::open(path).map_err(|_| bad("cannot be opened"))?;
    let meta = file.metadata().map_err(|_| bad("cannot be read"))?;
    if !meta.is_file() {
        return Err(bad("is not a regular file"));
    }
    if meta.mode() & 0o007 != 0 {
        return Err(bad("must not be readable by others (chmod 0400)"));
    }
    if meta.len() > MAX_TOKEN_FILE_BYTES {
        return Err(bad("is too large to be a token"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_TOKEN_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| bad("cannot be read"))?;
    let text = String::from_utf8(bytes).map_err(|_| bad("is not text"))?;
    let token = text.trim();
    if !(MIN_TOKEN_CHARS..=MAX_TOKEN_CHARS).contains(&token.len()) {
        return Err(bad("must hold 20 to 128 characters"));
    }
    if !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad("must hold printable ASCII only"));
    }
    Ok(Secret::new(token))
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

    use std::{fs, os::unix::fs::OpenOptionsExt};

    use crate::worker::state::testutil::Dir;

    const RD_TOKEN: &str = "test-token-0000-ABCDEFGHIJKL";

    /// Writes a token file with the given mode into `dir`.
    fn token_file(dir: &Dir, contents: &[u8], mode: u32) -> String {
        use std::io::Write;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.0.join(format!("token-{n}"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&path)
            .unwrap();
        file.write_all(contents).unwrap();
        // `mode` is filtered by the umask; set it exactly.
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode)).unwrap();
        path.to_str().unwrap().to_string()
    }

    fn load_rd(file: &str, extra: &[(&str, &str)]) -> Result<WorkerConfig, WorkerConfigError> {
        let mut pairs = vec![("INTERNAL_TOKEN", TOKEN), ("RD_TOKEN_FILE", file)];
        pairs.extend_from_slice(extra);
        load(&pairs)
    }

    fn rd_error(result: Result<WorkerConfig, WorkerConfigError>, name: &str) {
        match result {
            Err(WorkerConfigError::Invalid { name: n, reason }) => {
                assert_eq!(n, name, "{reason}");
                assert!(!reason.contains("test-token"), "{reason}");
            }
            other => panic!("expected {name} to be refused, got {other:?}"),
        }
    }

    #[test]
    fn without_a_token_file_there_is_no_rd_runtime_and_the_other_rd_settings_are_ignored() {
        let config = load(&[
            ("INTERNAL_TOKEN", TOKEN),
            ("WORKER_MAX_ADDS_PER_DAY", "lots"),
            ("RD_BASE_URL", "ftp://nowhere"),
        ])
        .unwrap();
        assert!(config.rd.is_none());
        assert!(
            load(&[("INTERNAL_TOKEN", TOKEN), ("RD_TOKEN_FILE", "  ")])
                .unwrap()
                .rd
                .is_none()
        );
    }

    #[test]
    fn a_token_file_gives_the_documented_defaults() {
        let dir = Dir::new();
        let file = token_file(&dir, format!("{RD_TOKEN}\n").as_bytes(), 0o400);
        let rd = load_rd(&file, &[]).unwrap().rd.unwrap();
        assert_eq!(rd.token.expose(), RD_TOKEN);
        assert_eq!(rd.base_url, "https://api.real-debrid.com/rest/1.0");
        assert_eq!(rd.state_dir, PathBuf::from("/state"));
        assert_eq!(rd.max_adds_per_day, 5);
        assert_eq!(rd.max_uncached_per_day, 2);
        assert_eq!(rd.max_bytes, 30 * GIB);
        assert_eq!(rd.per_min, 20);
        assert_eq!(rd.keep_free_slots, 2);
    }

    #[test]
    fn the_rd_settings_can_be_overridden() {
        let dir = Dir::new();
        let file = token_file(&dir, RD_TOKEN.as_bytes(), 0o440);
        let rd = load_rd(
            &file,
            &[
                ("RD_BASE_URL", "http://127.0.0.1:9999/rest/1.0/"),
                ("WORKER_STATE_DIR", "/var/lib/worker"),
                ("WORKER_MAX_ADDS_PER_DAY", "0"),
                ("WORKER_MAX_UNCACHED_PER_DAY", "7"),
                ("WORKER_RD_MAX_GB", "12"),
                ("WORKER_RD_PER_MIN", "30"),
                ("WORKER_KEEP_FREE_SLOTS", "0"),
            ],
        )
        .unwrap()
        .rd
        .unwrap();
        assert_eq!(rd.base_url, "http://127.0.0.1:9999/rest/1.0");
        assert_eq!(rd.state_dir, PathBuf::from("/var/lib/worker"));
        assert_eq!(
            (
                rd.max_adds_per_day,
                rd.max_uncached_per_day,
                rd.keep_free_slots
            ),
            (0, 7, 0)
        );
        assert_eq!((rd.max_bytes, rd.per_min), (12 * GIB, 30));
    }

    #[test]
    fn a_token_file_others_can_read_is_refused() {
        let dir = Dir::new();
        for mode in [0o644, 0o604, 0o601, 0o666] {
            let file = token_file(&dir, RD_TOKEN.as_bytes(), mode);
            rd_error(load_rd(&file, &[]), "RD_TOKEN_FILE");
        }
        // Group access is the container's business; only "others" count.
        let file = token_file(&dir, RD_TOKEN.as_bytes(), 0o640);
        assert!(load_rd(&file, &[]).is_ok());
    }

    #[test]
    fn a_bad_token_is_refused_without_quoting_it() {
        let dir = Dir::new();
        let long = "A".repeat(129);
        let cases: [&[u8]; 8] = [
            b"",
            b"   \n",
            b"tooshort-0123456789",
            long.as_bytes(),
            "test-token-åäö-0123456789".as_bytes(),
            b"test-token with space 0123",
            b"test-token\tTAB-0123456789",
            b"\xff\xfe\xfd\xfctest-token-0000-ABCDEFGH",
        ];
        for contents in cases {
            let file = token_file(&dir, contents, 0o400);
            rd_error(load_rd(&file, &[]), "RD_TOKEN_FILE");
        }
        // The edges are fine.
        let file = token_file(&dir, "A".repeat(20).as_bytes(), 0o400);
        assert!(load_rd(&file, &[]).is_ok());
        let file = token_file(&dir, "A".repeat(128).as_bytes(), 0o400);
        assert!(load_rd(&file, &[]).is_ok());
    }

    #[test]
    fn a_missing_oversized_or_non_file_token_path_is_refused() {
        let dir = Dir::new();
        rd_error(load_rd("/no/such/token", &[]), "RD_TOKEN_FILE");
        rd_error(load_rd(dir.0.to_str().unwrap(), &[]), "RD_TOKEN_FILE");
        let huge = token_file(&dir, &vec![b'A'; 5000], 0o400);
        rd_error(load_rd(&huge, &[]), "RD_TOKEN_FILE");
    }

    #[test]
    fn the_rd_base_url_must_be_https_or_loopback_http() {
        let dir = Dir::new();
        let file = token_file(&dir, RD_TOKEN.as_bytes(), 0o400);
        for good in ["https://api.example.invalid/rest", "http://127.0.0.1:8080"] {
            assert!(load_rd(&file, &[("RD_BASE_URL", good)]).is_ok(), "{good}");
        }
        for bad in [
            "http://api.example.invalid",
            "http://127.0.0.1.example.invalid",
            "http://localhost:8080",
            "https://user:pw@api.example.invalid",
            "api.example.invalid",
            "ftp://127.0.0.1",
        ] {
            rd_error(load_rd(&file, &[("RD_BASE_URL", bad)]), "RD_BASE_URL");
        }
    }

    #[test]
    fn rd_caps_above_the_maximum_or_below_the_minimum_are_refused() {
        let dir = Dir::new();
        let file = token_file(&dir, RD_TOKEN.as_bytes(), 0o400);
        for (name, bad) in [
            ("WORKER_MAX_ADDS_PER_DAY", "51"),
            ("WORKER_MAX_ADDS_PER_DAY", "-1"),
            ("WORKER_MAX_UNCACHED_PER_DAY", "21"),
            ("WORKER_RD_MAX_GB", "0"),
            ("WORKER_RD_MAX_GB", "101"),
            ("WORKER_RD_PER_MIN", "0"),
            ("WORKER_RD_PER_MIN", "31"),
            ("WORKER_KEEP_FREE_SLOTS", "21"),
            ("WORKER_KEEP_FREE_SLOTS", "two"),
        ] {
            rd_error(load_rd(&file, &[(name, bad)]), name);
        }
    }

    #[test]
    fn rd_token_in_the_environment_is_refused_even_with_a_token_file() {
        let dir = Dir::new();
        let file = token_file(&dir, RD_TOKEN.as_bytes(), 0o400);
        rd_error(load_rd(&file, &[("RD_TOKEN", RD_TOKEN)]), "RD_TOKEN");
    }

    #[test]
    fn debug_shows_no_rd_token() {
        let dir = Dir::new();
        let file = token_file(&dir, RD_TOKEN.as_bytes(), 0o400);
        let config = load_rd(&file, &[]).unwrap();
        let shown = format!("{config:?} {:#?}", config.rd);
        assert!(!shown.contains("test-token"));
        assert!(shown.contains("redacted"));
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
        assert_eq!(config.rank.pro_groups, ["framestor"], "the default");
        // Everything else keeps its default.
        let default = RankSettings::default();
        assert_eq!(config.rank.resolution, default.resolution);
        assert_eq!(config.rank.shortlist_len, default.shortlist_len);
    }

    #[test]
    fn the_preferred_pro_groups_are_read_lowercase_and_may_be_empty() {
        let groups = |text: &str| {
            load(&[("INTERNAL_TOKEN", TOKEN), ("WORKER_PRO_GROUPS", text)])
                .unwrap()
                .rank
                .pro_groups
        };
        assert_eq!(groups("FraMeStor, Example,,"), ["framestor", "example"]);
        assert!(groups(" , ").is_empty());
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
    fn the_auto_rule_can_be_overridden_within_range() {
        let config = load(&[
            ("INTERNAL_TOKEN", TOKEN),
            ("WORKER_AUTO_MIN_SCORE", "90.5"),
            ("WORKER_AUTO_MIN_SEEDERS", "25"),
            ("WORKER_AUTO_MAX_TRIES", "5"),
            ("WORKER_AUTO_MAX_GB", "100"),
        ])
        .unwrap();
        assert_eq!(config.rank.auto_min_score, 90.5);
        assert_eq!(config.rank.auto_min_seeders, 25);
        assert_eq!(config.rank.auto_max_tries, 5);
        assert_eq!(config.rank.auto_max_bytes, 100 * GIB);
        let low = load(&[
            ("INTERNAL_TOKEN", TOKEN),
            ("WORKER_AUTO_MAX_TRIES", "1"),
            ("WORKER_AUTO_MAX_GB", "1"),
        ])
        .unwrap();
        assert_eq!((low.rank.auto_max_tries, low.rank.auto_max_bytes), (1, GIB));
    }

    #[test]
    fn bad_auto_overrides_are_refused() {
        for (name, bad) in [
            ("WORKER_AUTO_MIN_SCORE", "-1"),
            ("WORKER_AUTO_MIN_SCORE", "NaN"),
            ("WORKER_AUTO_MIN_SEEDERS", "many"),
            ("WORKER_AUTO_MIN_SEEDERS", "5000000000"),
            ("WORKER_AUTO_MAX_TRIES", "0"),
            ("WORKER_AUTO_MAX_TRIES", "6"),
            ("WORKER_AUTO_MAX_TRIES", "three"),
            ("WORKER_AUTO_MAX_GB", "0"),
            ("WORKER_AUTO_MAX_GB", "101"),
            ("WORKER_AUTO_MAX_GB", "1.5"),
        ] {
            assert!(
                load(&[("INTERNAL_TOKEN", TOKEN), (name, bad)]).is_err(),
                "{name}={bad}"
            );
        }
    }

    #[test]
    fn debug_does_not_show_the_token() {
        let config = load(&[("INTERNAL_TOKEN", TOKEN)]).unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains(TOKEN));
        assert!(shown.contains("<redacted>"));
    }
}
