//! Startup configuration, read from the environment and validated once.
//!
//! Each phase adds the env vars it needs here; nothing is read from the
//! environment anywhere else in the codebase.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use thiserror::Error;

use crate::clientip::{Cidr, parse_cidr_list};

/// Below this, a leaked pepper plus a leaked database would make the HMAC
/// trivially reversible for short codes; see docs/SPEC.md "Access codes".
const MIN_PEPPER_BYTES: usize = 32;

/// The spec: "Codes shorter than 8 characters are not supported: a 6-digit
/// code has only a million values, which a distributed attacker could cover."
const MIN_CODE_LENGTH: usize = 8;

const DEFAULT_TMDB_BASE_URL: &str = "https://api.themoviedb.org/3";
const DEFAULT_DMM_BASE_URL: &str = "https://debridmediamanager.com";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid value for {name}: {reason}")]
    Invalid { name: &'static str, reason: String },
}

#[derive(Clone)]
pub struct Config {
    pub public_origin: String,
    pub bind_addr: SocketAddr,
    pub database_path: PathBuf,
    /// HMAC key for access codes. Never logged or debug-printed.
    pub code_pepper: Vec<u8>,
    pub cookie_dev_insecure: bool,
    pub code_length: usize,
    pub default_max_sessions: i64,
    pub default_daily_limit: i64,
    pub admin_user: String,
    /// Argon2id PHC hash string from `media-requests hash-password`. Not a
    /// secret itself (it's a salted hash), but never logged regardless.
    pub admin_password_hash: String,
    pub trusted_proxy_cidrs: Vec<Cidr>,
    /// TMDB API read access token, sent as a bearer token. Never logged.
    pub tmdb_token: String,
    /// Without a trailing slash, e.g. `https://api.themoviedb.org/3`.
    pub tmdb_base_url: String,
    /// Debrid Media Manager, for the admin page's "Open in DMM" links
    /// (`/movie/<imdb_id>`, `/show/<imdb_id>`). No trailing slash.
    pub dmm_base_url: String,
    pub ntfy: NtfyConfig,
    pub rate_limits: RateLimitConfig,
}

/// Where push notifications go. `url: None` turns them off.
#[derive(Clone, Default)]
pub struct NtfyConfig {
    pub url: Option<String>,
    pub topic: String,
    /// Optional bearer token for a self-hosted ntfy. Never logged.
    pub token: Option<String>,
}

impl std::fmt::Debug for NtfyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NtfyConfig")
            .field("url", &self.url)
            // Anyone who knows a public ntfy.sh topic can read it, so the
            // topic is treated as a secret too.
            .field("topic", &"<redacted>")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Global failure throttle for one kind of login (access code or admin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThrottleConfig {
    pub window_secs: u64,
    /// More failures than this within the window delays attempts by
    /// `delay1_ms`...
    pub tier1: u32,
    /// ...and more than this by `delay2_ms`.
    pub tier2: u32,
}

/// Every number from the spec's rate-limit table, overridable via `RL_*`
/// env vars. `Default` holds the spec's values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub api_per_minute: u32,
    pub search_burst: u32,
    pub search_per_second: u32,
    /// Global cap on outbound TMDB calls.
    pub tmdb_per_second: u32,
    /// New requests (and duplicates) per member per minute. The daily cap is
    /// `DEFAULT_DAILY_LIMIT` or the member's own override.
    pub requests_per_minute: u32,
    /// Hard cap on tracked keys per limiter; new keys beyond it are limited.
    pub key_cap: usize,
    pub fail_max: u32,
    pub fail_window_secs: u64,
    /// Lockout length for each successive backoff level.
    pub lockout_secs: Vec<u64>,
    pub code_throttle: ThrottleConfig,
    pub admin_throttle: ThrottleConfig,
    pub delay1_ms: u64,
    pub delay2_ms: u64,
    /// Permits for attempts waiting out a throttle delay, per throttle.
    pub delay_permits: u32,
    /// Minimum gap between two throttle on/off ntfy alerts.
    pub alert_interval_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            api_per_minute: 120,
            search_burst: 10,
            search_per_second: 5,
            tmdb_per_second: 20,
            requests_per_minute: 3,
            key_cap: 10_000,
            fail_max: 5,
            fail_window_secs: 15 * 60,
            lockout_secs: vec![15 * 60, 3600, 24 * 3600],
            code_throttle: ThrottleConfig {
                window_secs: 3600,
                tier1: 60,
                tier2: 300,
            },
            admin_throttle: ThrottleConfig {
                window_secs: 15 * 60,
                tier1: 30,
                tier2: 150,
            },
            delay1_ms: 1000,
            delay2_ms: 3000,
            delay_permits: 32,
            alert_interval_secs: 3600,
        }
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("public_origin", &self.public_origin)
            .field("bind_addr", &self.bind_addr)
            .field("database_path", &self.database_path)
            .field("code_pepper", &"<redacted>")
            .field("cookie_dev_insecure", &self.cookie_dev_insecure)
            .field("code_length", &self.code_length)
            .field("default_max_sessions", &self.default_max_sessions)
            .field("default_daily_limit", &self.default_daily_limit)
            .field("admin_user", &self.admin_user)
            .field("admin_password_hash", &"<redacted>")
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
            .field("tmdb_token", &"<redacted>")
            .field("tmdb_base_url", &self.tmdb_base_url)
            .field("dmm_base_url", &self.dmm_base_url)
            .field("ntfy", &self.ntfy)
            .field("rate_limits", &self.rate_limits)
            .finish()
    }
}

/// Minimal lookup abstraction so tests can supply a fake environment
/// instead of mutating the process-wide (and, since edition 2024, `unsafe`)
/// environment table.
trait EnvLookup {
    fn get(&self, key: &str) -> Option<String>;
}

struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl EnvLookup for HashMap<&str, &str> {
    fn get(&self, key: &str) -> Option<String> {
        HashMap::get(self, key).map(|v| v.to_string())
    }
}

fn required(env: &impl EnvLookup, name: &'static str) -> Result<String, ConfigError> {
    env.get(name).ok_or(ConfigError::Missing(name))
}

fn with_default(env: &impl EnvLookup, name: &str, default: &str) -> String {
    env.get(name).unwrap_or_else(|| default.to_string())
}

fn bool_flag(env: &impl EnvLookup, name: &str, default: bool) -> bool {
    match env.get(name) {
        Some(v) => v == "1" || v.eq_ignore_ascii_case("true"),
        None => default,
    }
}

fn int_with_default(
    env: &impl EnvLookup,
    name: &'static str,
    default: i64,
) -> Result<i64, ConfigError> {
    match env.get(name) {
        Some(v) => v.parse().map_err(|_| ConfigError::Invalid {
            name,
            reason: "must be an integer".into(),
        }),
        None => Ok(default),
    }
}

/// A rate-limit number: a positive integer, since a zero would either
/// disable a limit or make it reject everything.
fn positive<T: std::str::FromStr + PartialOrd + From<u8>>(
    env: &impl EnvLookup,
    name: &'static str,
    default: T,
) -> Result<T, ConfigError> {
    let Some(raw) = env.get(name) else {
        return Ok(default);
    };
    match raw.trim().parse::<T>() {
        Ok(v) if v >= T::from(1) => Ok(v),
        _ => Err(ConfigError::Invalid {
            name,
            reason: "must be a positive integer".into(),
        }),
    }
}

fn throttle_config(
    env: &impl EnvLookup,
    names: [&'static str; 3],
    default: ThrottleConfig,
) -> Result<ThrottleConfig, ConfigError> {
    let [window_name, tier1_name, tier2_name] = names;
    let cfg = ThrottleConfig {
        window_secs: positive(env, window_name, default.window_secs)?,
        tier1: positive(env, tier1_name, default.tier1)?,
        tier2: positive(env, tier2_name, default.tier2)?,
    };
    if cfg.tier2 <= cfg.tier1 {
        return Err(ConfigError::Invalid {
            name: tier2_name,
            reason: format!("must be greater than {tier1_name}"),
        });
    }
    Ok(cfg)
}

fn rate_limit_config(env: &impl EnvLookup) -> Result<RateLimitConfig, ConfigError> {
    let d = RateLimitConfig::default();

    let lockout_secs = match env.get("RL_LOCKOUT_SECS") {
        None => d.lockout_secs,
        Some(raw) => {
            let parsed: Option<Vec<u64>> = raw
                .split(',')
                .map(|s| s.trim().parse::<u64>().ok().filter(|v| *v >= 1))
                .collect();
            parsed
                .filter(|v| !v.is_empty())
                .ok_or(ConfigError::Invalid {
                    name: "RL_LOCKOUT_SECS",
                    reason: "must be a comma-separated list of positive integers".into(),
                })?
        }
    };

    let cfg = RateLimitConfig {
        api_per_minute: positive(env, "RL_API_PER_MINUTE", d.api_per_minute)?,
        search_burst: positive(env, "RL_SEARCH_BURST", d.search_burst)?,
        search_per_second: positive(env, "RL_SEARCH_PER_SECOND", d.search_per_second)?,
        tmdb_per_second: positive(env, "RL_TMDB_PER_SECOND", d.tmdb_per_second)?,
        requests_per_minute: positive(env, "RL_REQUESTS_PER_MINUTE", d.requests_per_minute)?,
        key_cap: positive(env, "RL_KEY_CAP", d.key_cap as u64)? as usize,
        fail_max: positive(env, "RL_FAIL_MAX", d.fail_max)?,
        fail_window_secs: positive(env, "RL_FAIL_WINDOW_SECS", d.fail_window_secs)?,
        lockout_secs,
        code_throttle: throttle_config(
            env,
            [
                "RL_CODE_THROTTLE_WINDOW_SECS",
                "RL_CODE_THROTTLE_TIER1",
                "RL_CODE_THROTTLE_TIER2",
            ],
            d.code_throttle,
        )?,
        admin_throttle: throttle_config(
            env,
            [
                "RL_ADMIN_THROTTLE_WINDOW_SECS",
                "RL_ADMIN_THROTTLE_TIER1",
                "RL_ADMIN_THROTTLE_TIER2",
            ],
            d.admin_throttle,
        )?,
        delay1_ms: positive(env, "RL_THROTTLE_DELAY1_MS", d.delay1_ms)?,
        delay2_ms: positive(env, "RL_THROTTLE_DELAY2_MS", d.delay2_ms)?,
        delay_permits: positive(env, "RL_DELAY_PERMITS", d.delay_permits)?,
        alert_interval_secs: positive(env, "RL_ALERT_INTERVAL_SECS", d.alert_interval_secs)?,
    };
    Ok(cfg)
}

fn ntfy_config(env: &impl EnvLookup) -> Result<NtfyConfig, ConfigError> {
    let url = env
        .get("NTFY_URL")
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty());
    let Some(url) = url else {
        return Ok(NtfyConfig::default());
    };
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(ConfigError::Invalid {
            name: "NTFY_URL",
            reason: "must start with http:// or https://".into(),
        });
    }
    let topic = env
        .get("NTFY_TOPIC")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::Missing("NTFY_TOPIC"))?;
    // The topic becomes a URL path segment; keep it to the characters
    // ntfy itself allows so it can never alter the request path.
    if !topic
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(ConfigError::Invalid {
            name: "NTFY_TOPIC",
            reason: "may only contain letters, digits, '-' and '_'".into(),
        });
    }
    let token = env
        .get("NTFY_TOKEN")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    Ok(NtfyConfig {
        url: Some(url),
        topic,
        token,
    })
}

impl Config {
    /// Reads and validates configuration from the process environment.
    /// Fails fast with a specific, non-secret-leaking message.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(&ProcessEnv)
    }

    fn from_lookup(env: &impl EnvLookup) -> Result<Self, ConfigError> {
        let public_origin = required(env, "PUBLIC_ORIGIN")?;
        let cookie_dev_insecure = bool_flag(env, "COOKIE_DEV_INSECURE", false);

        let is_https = public_origin.starts_with("https://");
        let is_http = public_origin.starts_with("http://");
        if !is_https && !is_http {
            return Err(ConfigError::Invalid {
                name: "PUBLIC_ORIGIN",
                reason: "must start with http:// or https://".into(),
            });
        }
        if !is_https && !cookie_dev_insecure {
            return Err(ConfigError::Invalid {
                name: "PUBLIC_ORIGIN",
                reason: "must be https unless COOKIE_DEV_INSECURE=1".into(),
            });
        }
        if is_https && cookie_dev_insecure {
            return Err(ConfigError::Invalid {
                name: "COOKIE_DEV_INSECURE",
                reason: "refusing to start with an insecure cookie flag against an https origin"
                    .into(),
            });
        }

        let bind_addr_raw = with_default(env, "BIND_ADDR", "0.0.0.0:8000");
        let bind_addr = bind_addr_raw
            .parse::<SocketAddr>()
            .map_err(|e| ConfigError::Invalid {
                name: "BIND_ADDR",
                reason: e.to_string(),
            })?;

        let database_path = PathBuf::from(with_default(env, "DATABASE_PATH", "./data/db.sqlite3"));

        let code_pepper = required(env, "CODE_PEPPER")?.into_bytes();
        if code_pepper.len() < MIN_PEPPER_BYTES {
            return Err(ConfigError::Invalid {
                name: "CODE_PEPPER",
                reason: format!("must be at least {MIN_PEPPER_BYTES} bytes"),
            });
        }

        let code_length_raw = int_with_default(env, "CODE_LENGTH", MIN_CODE_LENGTH as i64)?;
        if code_length_raw < MIN_CODE_LENGTH as i64 {
            return Err(ConfigError::Invalid {
                name: "CODE_LENGTH",
                reason: format!("must be at least {MIN_CODE_LENGTH}"),
            });
        }
        let code_length = code_length_raw as usize;

        let default_max_sessions = int_with_default(env, "DEFAULT_MAX_SESSIONS", 3)?;
        if default_max_sessions < 1 {
            return Err(ConfigError::Invalid {
                name: "DEFAULT_MAX_SESSIONS",
                reason: "must be at least 1".into(),
            });
        }

        let default_daily_limit = int_with_default(env, "DEFAULT_DAILY_LIMIT", 10)?;
        if default_daily_limit < 0 {
            return Err(ConfigError::Invalid {
                name: "DEFAULT_DAILY_LIMIT",
                reason: "must not be negative".into(),
            });
        }

        let admin_user = required(env, "ADMIN_USER")?;
        let admin_password_hash = required(env, "ADMIN_PASSWORD_HASH")?;
        // Fail fast on a malformed hash rather than discovering it on the
        // first login attempt; the parsed value itself is discarded.
        argon2::password_hash::phc::PasswordHash::new(&admin_password_hash).map_err(|e| {
            ConfigError::Invalid {
                name: "ADMIN_PASSWORD_HASH",
                reason: format!("not a valid PHC hash string: {e}"),
            }
        })?;

        let trusted_proxy_cidrs_raw = with_default(
            env,
            "TRUSTED_PROXY_CIDRS",
            "127.0.0.1/32,::1/128,172.16.0.0/12",
        );
        let trusted_proxy_cidrs =
            parse_cidr_list(&trusted_proxy_cidrs_raw).map_err(|reason| ConfigError::Invalid {
                name: "TRUSTED_PROXY_CIDRS",
                reason,
            })?;

        let tmdb_token = required(env, "TMDB_TOKEN")?.trim().to_string();
        if tmdb_token.is_empty() {
            return Err(ConfigError::Missing("TMDB_TOKEN"));
        }
        let tmdb_base_url = with_default(env, "TMDB_BASE_URL", DEFAULT_TMDB_BASE_URL)
            .trim()
            .trim_end_matches('/')
            .to_string();
        if !tmdb_base_url.starts_with("https://") && !tmdb_base_url.starts_with("http://") {
            return Err(ConfigError::Invalid {
                name: "TMDB_BASE_URL",
                reason: "must start with http:// or https://".into(),
            });
        }

        let dmm_base_url = with_default(env, "DMM_BASE_URL", DEFAULT_DMM_BASE_URL)
            .trim()
            .trim_end_matches('/')
            .to_string();
        if !dmm_base_url.starts_with("https://") && !dmm_base_url.starts_with("http://") {
            return Err(ConfigError::Invalid {
                name: "DMM_BASE_URL",
                reason: "must start with http:// or https://".into(),
            });
        }

        let ntfy = ntfy_config(env)?;
        let rate_limits = rate_limit_config(env)?;

        Ok(Config {
            public_origin,
            bind_addr,
            database_path,
            code_pepper,
            cookie_dev_insecure,
            code_length,
            default_max_sessions,
            default_daily_limit,
            admin_user,
            admin_password_hash,
            trusted_proxy_cidrs,
            tmdb_token,
            tmdb_base_url,
            dmm_base_url,
            ntfy,
            rate_limits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn long_pepper() -> &'static str {
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" // 32 bytes
    }

    /// A real Argon2id PHC string, computed once, so config validation
    /// tests exercise the actual parser instead of a hand-typed fixture.
    fn valid_password_hash() -> &'static str {
        static HASH: OnceLock<String> = OnceLock::new();
        HASH.get_or_init(|| {
            use argon2::password_hash::PasswordHasher;
            argon2::Argon2::default()
                .hash_password(b"test-password")
                .expect("hash a fixed test password")
                .to_string()
        })
    }

    fn base_env() -> HashMap<&'static str, &'static str> {
        HashMap::from([
            ("PUBLIC_ORIGIN", "https://example.com"),
            ("CODE_PEPPER", long_pepper()),
            ("ADMIN_USER", "admin"),
            ("ADMIN_PASSWORD_HASH", valid_password_hash()),
            ("TMDB_TOKEN", "secret-tmdb-token"),
        ])
    }

    #[test]
    fn rejects_missing_public_origin() {
        let mut env = base_env();
        env.remove("PUBLIC_ORIGIN");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("PUBLIC_ORIGIN")));
    }

    #[test]
    fn rejects_http_origin_without_dev_flag() {
        let mut env = base_env();
        env.insert("PUBLIC_ORIGIN", "http://example.com");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "PUBLIC_ORIGIN",
                ..
            }
        ));
    }

    #[test]
    fn accepts_http_origin_with_dev_flag() {
        let mut env = base_env();
        env.insert("PUBLIC_ORIGIN", "http://localhost:8000");
        env.insert("COOKIE_DEV_INSECURE", "1");
        let cfg = Config::from_lookup(&env).unwrap();
        assert!(cfg.cookie_dev_insecure);
    }

    #[test]
    fn rejects_dev_flag_with_https_origin() {
        let mut env = base_env();
        env.insert("COOKIE_DEV_INSECURE", "1");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "COOKIE_DEV_INSECURE",
                ..
            }
        ));
    }

    #[test]
    fn rejects_short_pepper() {
        let mut env = base_env();
        env.insert("CODE_PEPPER", "too-short");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "CODE_PEPPER",
                ..
            }
        ));
    }

    #[test]
    fn accepts_valid_config() {
        let mut env = base_env();
        env.insert("BIND_ADDR", "127.0.0.1:9000");
        let cfg = Config::from_lookup(&env).unwrap();
        assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1:9000");
        assert_eq!(cfg.public_origin, "https://example.com");
        assert_eq!(cfg.code_length, 8);
        assert_eq!(cfg.default_max_sessions, 3);
        assert_eq!(cfg.default_daily_limit, 10);
        assert_eq!(cfg.trusted_proxy_cidrs.len(), 3);
    }

    #[test]
    fn rejects_code_length_below_minimum() {
        let mut env = base_env();
        env.insert("CODE_LENGTH", "6");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "CODE_LENGTH",
                ..
            }
        ));
    }

    #[test]
    fn rejects_missing_admin_user() {
        let mut env = base_env();
        env.remove("ADMIN_USER");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("ADMIN_USER")));
    }

    #[test]
    fn rejects_malformed_admin_password_hash() {
        let mut env = base_env();
        env.insert("ADMIN_PASSWORD_HASH", "not-a-phc-string");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "ADMIN_PASSWORD_HASH",
                ..
            }
        ));
    }

    #[test]
    fn tmdb_token_is_required_and_base_url_defaults() {
        let cfg = Config::from_lookup(&base_env()).unwrap();
        assert_eq!(cfg.tmdb_base_url, "https://api.themoviedb.org/3");
        assert_eq!(cfg.dmm_base_url, "https://debridmediamanager.com");

        let mut env = base_env();
        env.insert("TMDB_BASE_URL", "http://127.0.0.1:9999/");
        let cfg = Config::from_lookup(&env).unwrap();
        assert_eq!(cfg.tmdb_base_url, "http://127.0.0.1:9999");

        env.insert("TMDB_TOKEN", "  ");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("TMDB_TOKEN")));
    }

    #[test]
    fn rejects_bad_trusted_proxy_cidrs() {
        let mut env = base_env();
        env.insert("TRUSTED_PROXY_CIDRS", "garbage");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "TRUSTED_PROXY_CIDRS",
                ..
            }
        ));
    }

    #[test]
    fn debug_does_not_leak_secrets() {
        let mut env = base_env();
        env.insert("NTFY_URL", "https://ntfy.example.com");
        env.insert("NTFY_TOPIC", "secret-topic-name");
        env.insert("NTFY_TOKEN", "secret-ntfy-token");
        let cfg = Config::from_lookup(&env).unwrap();
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("test-password"));
        assert!(!debug.contains(long_pepper()));
        assert!(!debug.contains("secret-topic-name"));
        assert!(!debug.contains("secret-ntfy-token"));
        assert!(!debug.contains("secret-tmdb-token"));
    }

    #[test]
    fn rate_limits_default_to_spec_values() {
        let cfg = Config::from_lookup(&base_env()).unwrap();
        let rl = &cfg.rate_limits;
        assert_eq!(*rl, RateLimitConfig::default());
        assert_eq!(rl.api_per_minute, 120);
        assert_eq!((rl.search_burst, rl.search_per_second), (10, 5));
        assert_eq!(rl.key_cap, 10_000);
        assert_eq!((rl.fail_max, rl.fail_window_secs), (5, 900));
        assert_eq!(rl.lockout_secs, vec![900, 3600, 86_400]);
        assert_eq!((rl.code_throttle.tier1, rl.code_throttle.tier2), (60, 300));
        assert_eq!(rl.code_throttle.window_secs, 3600);
        assert_eq!(rl.admin_throttle.tier1, 30);
        assert_eq!(rl.admin_throttle.window_secs, 900);
        assert_eq!((rl.delay1_ms, rl.delay2_ms), (1000, 3000));
        assert_eq!(rl.delay_permits, 32);
    }

    #[test]
    fn rate_limits_are_overridable() {
        let mut env = base_env();
        env.insert("RL_API_PER_MINUTE", "300");
        env.insert("RL_LOCKOUT_SECS", "60, 120");
        env.insert("RL_CODE_THROTTLE_TIER1", "10");
        env.insert("RL_CODE_THROTTLE_TIER2", "20");
        let rl = Config::from_lookup(&env).unwrap().rate_limits;
        assert_eq!(rl.api_per_minute, 300);
        assert_eq!(rl.lockout_secs, vec![60, 120]);
        assert_eq!((rl.code_throttle.tier1, rl.code_throttle.tier2), (10, 20));
    }

    #[test]
    fn rejects_zero_or_garbage_rate_limits() {
        for (name, value) in [
            ("RL_API_PER_MINUTE", "0"),
            ("RL_SEARCH_BURST", "-1"),
            ("RL_KEY_CAP", "lots"),
            ("RL_LOCKOUT_SECS", ""),
            ("RL_LOCKOUT_SECS", "900,0"),
        ] {
            let mut env = base_env();
            env.insert(name, value);
            let err = Config::from_lookup(&env).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { name: n, .. } if n == name),
                "{name}={value:?} gave {err:?}"
            );
        }
    }

    #[test]
    fn rejects_throttle_tiers_out_of_order() {
        let mut env = base_env();
        env.insert("RL_ADMIN_THROTTLE_TIER1", "50");
        env.insert("RL_ADMIN_THROTTLE_TIER2", "50");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "RL_ADMIN_THROTTLE_TIER2",
                ..
            }
        ));
    }

    #[test]
    fn ntfy_is_off_without_url_and_needs_a_topic_with_one() {
        let cfg = Config::from_lookup(&base_env()).unwrap();
        assert!(cfg.ntfy.url.is_none());

        let mut env = base_env();
        env.insert("NTFY_URL", "https://ntfy.sh/");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("NTFY_TOPIC")));

        env.insert("NTFY_TOPIC", "my-long-topic_1");
        let cfg = Config::from_lookup(&env).unwrap();
        assert_eq!(cfg.ntfy.url.as_deref(), Some("https://ntfy.sh"));
        assert_eq!(cfg.ntfy.topic, "my-long-topic_1");
        assert!(cfg.ntfy.token.is_none());
    }

    #[test]
    fn rejects_ntfy_topic_that_could_alter_the_path() {
        let mut env = base_env();
        env.insert("NTFY_URL", "https://ntfy.sh");
        env.insert("NTFY_TOPIC", "../admin");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "NTFY_TOPIC",
                ..
            }
        ));
    }
}
