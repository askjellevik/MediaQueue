//! Startup configuration, read from the environment and validated once.
//!
//! Each phase adds the env vars it needs here; nothing is read from the
//! environment anywhere else in the codebase.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use thiserror::Error;

/// Below this, a leaked pepper plus a leaked database would make the HMAC
/// trivially reversible for short codes; see docs/SPEC.md "Access codes".
const MIN_PEPPER_BYTES: usize = 32;

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
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("public_origin", &self.public_origin)
            .field("bind_addr", &self.bind_addr)
            .field("database_path", &self.database_path)
            .field("code_pepper", &"<redacted>")
            .field("cookie_dev_insecure", &self.cookie_dev_insecure)
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

        Ok(Config {
            public_origin,
            bind_addr,
            database_path,
            code_pepper,
            cookie_dev_insecure,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn long_pepper() -> &'static str {
        "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" // 32 bytes
    }

    #[test]
    fn rejects_missing_public_origin() {
        let env = HashMap::from([("CODE_PEPPER", long_pepper())]);
        let err = Config::from_lookup(&env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("PUBLIC_ORIGIN")));
    }

    #[test]
    fn rejects_http_origin_without_dev_flag() {
        let env = HashMap::from([
            ("PUBLIC_ORIGIN", "http://example.com"),
            ("CODE_PEPPER", long_pepper()),
        ]);
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
        let env = HashMap::from([
            ("PUBLIC_ORIGIN", "http://localhost:8000"),
            ("COOKIE_DEV_INSECURE", "1"),
            ("CODE_PEPPER", long_pepper()),
        ]);
        let cfg = Config::from_lookup(&env).unwrap();
        assert!(cfg.cookie_dev_insecure);
    }

    #[test]
    fn rejects_dev_flag_with_https_origin() {
        let env = HashMap::from([
            ("PUBLIC_ORIGIN", "https://example.com"),
            ("COOKIE_DEV_INSECURE", "1"),
            ("CODE_PEPPER", long_pepper()),
        ]);
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
        let env = HashMap::from([
            ("PUBLIC_ORIGIN", "https://example.com"),
            ("CODE_PEPPER", "too-short"),
        ]);
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
        let env = HashMap::from([
            ("PUBLIC_ORIGIN", "https://example.com"),
            ("CODE_PEPPER", long_pepper()),
            ("BIND_ADDR", "127.0.0.1:9000"),
        ]);
        let cfg = Config::from_lookup(&env).unwrap();
        assert_eq!(cfg.bind_addr.to_string(), "127.0.0.1:9000");
        assert_eq!(cfg.public_origin, "https://example.com");
    }

    #[test]
    fn debug_does_not_leak_pepper() {
        let cfg = Config {
            public_origin: "https://example.com".into(),
            bind_addr: "127.0.0.1:8000".parse().unwrap(),
            database_path: "./data/db.sqlite3".into(),
            code_pepper: b"super-secret-pepper-value-xxxxxx".to_vec(),
            cookie_dev_insecure: false,
        };
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("super-secret"));
    }
}
