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
        let env = base_env();
        let cfg = Config::from_lookup(&env).unwrap();
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("test-password"));
        assert!(!debug.contains(long_pepper()));
    }
}
