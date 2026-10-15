//! Startup configuration, read from the environment and validated once.
//!
//! Nothing is read from the environment anywhere else in the codebase.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use thiserror::Error;

use crate::clientip::{Cidr, parse_cidr_list};

/// Below this, a leaked pepper plus a leaked database would make the HMAC
/// trivially reversible for short codes.
const MIN_PEPPER_BYTES: usize = 32;

/// Devices a member may be logged in on at once. A device is just a login
/// session, so this only bounds session rows; it is not an anti-sharing
/// control.
pub const MAX_SESSIONS_PER_MEMBER: i64 = 10;

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
    pub admin_user: String,
    /// Argon2id PHC hash string from `mediaqueue hash-password`. Not a
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
    /// Cloudflare Access JWT check for `/admin` and `/api/admin`; `None` = off.
    pub cf_access: Option<CfAccessConfig>,
    pub ntfy: NtfyConfig,
    pub rate_limits: RateLimitConfig,
}

/// Cloudflare Access settings (`cf-access` feature).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfAccessConfig {
    /// Expected `iss`, e.g. `https://myteam.cloudflareaccess.com`.
    pub issuer: String,
    /// The Access application's AUD tag.
    pub audience: String,
    /// Where the team's signing keys are published (JWKS).
    pub certs_url: String,
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

/// Every rate-limit number. They only protect the server and TMDB, so they
/// are fixed rather than configurable; tests override fields directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub api_per_minute: u32,
    pub search_burst: u32,
    pub search_per_second: u32,
    /// Global cap on outbound TMDB calls.
    pub tmdb_per_second: u32,
    /// New requests (and duplicates) per member per minute. The daily cap is
    /// the member's own limit, if the admin set one.
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
            requests_per_minute: 10,
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
            .field("admin_user", &self.admin_user)
            .field("admin_password_hash", &"<redacted>")
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
            .field("tmdb_token", &"<redacted>")
            .field("tmdb_base_url", &self.tmdb_base_url)
            .field("dmm_base_url", &self.dmm_base_url)
            .field("cf_access", &self.cf_access)
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

fn cf_access_config(env: &impl EnvLookup) -> Result<Option<CfAccessConfig>, ConfigError> {
    if !bool_flag(env, "CF_ACCESS_ENABLED", false) {
        return Ok(None);
    }
    if !cfg!(feature = "cf-access") {
        return Err(ConfigError::Invalid {
            name: "CF_ACCESS_ENABLED",
            reason: "this build lacks the cf-access feature; rebuild with --features cf-access"
                .into(),
        });
    }
    let team = env
        .get("CF_ACCESS_TEAM_DOMAIN")
        .map(|v| {
            v.trim()
                .trim_start_matches("https://")
                .trim_end_matches('/')
                .to_string()
        })
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::Missing("CF_ACCESS_TEAM_DOMAIN"))?;
    if !team
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(ConfigError::Invalid {
            name: "CF_ACCESS_TEAM_DOMAIN",
            reason: "must be a host name such as myteam.cloudflareaccess.com".into(),
        });
    }
    let audience = env
        .get("CF_ACCESS_AUD")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or(ConfigError::Missing("CF_ACCESS_AUD"))?;
    let issuer = format!("https://{team}");
    let certs_url = with_default(
        env,
        "CF_ACCESS_CERTS_URL",
        &format!("{issuer}/cdn-cgi/access/certs"),
    );
    Ok(Some(CfAccessConfig {
        issuer,
        audience,
        certs_url,
    }))
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
        // The CSRF check compares this byte for byte with the browser's
        // `Origin`, which never ends in a slash.
        let public_origin = public_origin.trim().trim_end_matches('/').to_string();

        let cookie_dev_insecure = public_origin.starts_with("http://");
        if !cookie_dev_insecure && !public_origin.starts_with("https://") {
            return Err(ConfigError::Invalid {
                name: "PUBLIC_ORIGIN",
                reason: "must start with http:// or https://".into(),
            });
        }
        let authority = &public_origin[public_origin.find("://").map_or(0, |i| i + 3)..];
        if authority.is_empty() || authority.contains(['/', '?', '#']) {
            return Err(ConfigError::Invalid {
                name: "PUBLIC_ORIGIN",
                reason: "must be just scheme, host and optional port, for example https://requests.example.com (no path, query or fragment)".into(),
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

        let cf_access = cf_access_config(env)?;

        let ntfy = ntfy_config(env)?;

        Ok(Config {
            public_origin,
            bind_addr,
            database_path,
            code_pepper,
            cookie_dev_insecure,
            admin_user,
            admin_password_hash,
            trusted_proxy_cidrs,
            tmdb_token,
            tmdb_base_url,
            dmm_base_url,
            cf_access,
            ntfy,
            rate_limits: RateLimitConfig::default(),
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
    fn http_origin_enables_insecure_cookies() {
        let mut env = base_env();
        env.insert("PUBLIC_ORIGIN", "http://localhost:8000");
        let cfg = Config::from_lookup(&env).unwrap();
        assert!(cfg.cookie_dev_insecure);
    }

    #[test]
    fn https_origin_keeps_cookies_secure() {
        let cfg = Config::from_lookup(&base_env()).unwrap();
        assert!(!cfg.cookie_dev_insecure);
    }

    #[test]
    fn origin_must_be_http_or_https() {
        let mut env = base_env();
        env.insert("PUBLIC_ORIGIN", "example.com");
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
    fn origin_loses_a_trailing_slash_and_rejects_paths() {
        let mut env = base_env();
        env.insert("PUBLIC_ORIGIN", "https://example.com:8443/");
        let cfg = Config::from_lookup(&env).unwrap();
        assert_eq!(cfg.public_origin, "https://example.com:8443");

        for bad in [
            "https://example.com/app",
            "https://example.com?x=1",
            "https://example.com#top",
            "https://",
        ] {
            env.insert("PUBLIC_ORIGIN", bad);
            let err = Config::from_lookup(&env).unwrap_err();
            assert!(
                matches!(
                    err,
                    ConfigError::Invalid {
                        name: "PUBLIC_ORIGIN",
                        ..
                    }
                ),
                "{bad} gave {err:?}"
            );
        }
    }

    #[test]
    fn unknown_variables_are_ignored() {
        let mut env = base_env();
        env.insert("RL_FAIL_MAX", "1");
        env.insert("COOKIE_DEV_INSECURE", "1");
        let cfg = Config::from_lookup(&env).unwrap();
        assert_eq!(cfg.rate_limits, RateLimitConfig::default());
        assert!(!cfg.cookie_dev_insecure);
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
        assert_eq!(cfg.trusted_proxy_cidrs.len(), 3);
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
    fn cf_access_is_off_by_default() {
        let cfg = Config::from_lookup(&base_env()).unwrap();
        assert!(cfg.cf_access.is_none());
    }

    #[cfg(feature = "cf-access")]
    #[test]
    fn cf_access_needs_team_and_audience() {
        let mut env = base_env();
        env.insert("CF_ACCESS_ENABLED", "1");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("CF_ACCESS_TEAM_DOMAIN")));
        env.insert(
            "CF_ACCESS_TEAM_DOMAIN",
            "https://myteam.cloudflareaccess.com/",
        );
        env.insert("CF_ACCESS_AUD", "aud-tag");
        let cf = Config::from_lookup(&env).unwrap().cf_access.unwrap();
        assert_eq!(cf.issuer, "https://myteam.cloudflareaccess.com");
        assert_eq!(
            cf.certs_url,
            "https://myteam.cloudflareaccess.com/cdn-cgi/access/certs"
        );
    }

    #[cfg(not(feature = "cf-access"))]
    #[test]
    fn cf_access_switch_fails_fast_without_the_feature() {
        let mut env = base_env();
        env.insert("CF_ACCESS_ENABLED", "1");
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                name: "CF_ACCESS_ENABLED",
                ..
            }
        ));
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
    fn rate_limits_use_the_built_in_defaults() {
        let cfg = Config::from_lookup(&base_env()).unwrap();
        let rl = &cfg.rate_limits;
        assert_eq!(*rl, RateLimitConfig::default());
        assert_eq!(rl.api_per_minute, 120);
        assert_eq!(rl.requests_per_minute, 10);
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
