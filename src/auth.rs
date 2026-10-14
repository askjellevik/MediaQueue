//! Access codes, sessions, cookies and the `RequireUser`/`RequireAdmin`
//! extractors. See docs/SPEC.md "Authentication and security".

use std::sync::OnceLock;

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{
    extract::FromRequestParts,
    http::{HeaderValue, request::Parts},
};
use axum_extra::extract::cookie::CookieJar;
use hmac::{Hmac, KeyInit, Mac};
use rand::{Rng, RngExt};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{db, error::AppError, state::AppState};

// ---------------------------------------------------------------------
// Access codes
// ---------------------------------------------------------------------

/// 31 symbols, deliberately excluding look-alikes 0/O, 1/I/L.
const CODE_ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";

/// Generates a fresh access code of `length` characters using the OS-backed
/// CSPRNG's unbiased range sampling (no modulo bias).
pub fn generate_code(length: usize) -> String {
    let mut rng = rand::rng();
    (0..length)
        .map(|_| CODE_ALPHABET[rng.random_range(0..CODE_ALPHABET.len())] as char)
        .collect()
}

/// Formats a raw code for display, e.g. `ABCDEFGH` -> `ABCD-EFGH`. Purely
/// cosmetic; the dash is stripped again by [`normalise_code`].
pub fn format_code_for_display(code: &str) -> String {
    let mid = code.len() / 2;
    if mid == 0 || mid >= code.len() {
        return code.to_string();
    }
    format!("{}-{}", &code[..mid], &code[mid..])
}

/// Uppercases and strips spaces/hyphens. The alphabet never contains
/// look-alike characters, so a code typed with e.g. `O` instead of a digit
/// simply fails to match any stored hash — no special-casing needed.
pub fn normalise_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .flat_map(char::to_uppercase)
        .collect()
}

type HmacSha256 = Hmac<Sha256>;

/// `HMAC-SHA256(CODE_PEPPER, normalised_code)`. The pepper is used as the
/// MAC key directly; HMAC-SHA256 derives a fixed-size key from an input of
/// any length (padding or hashing it down), so `new_from_slice` never
/// actually fails here.
pub fn hmac_code(pepper: &[u8], normalised_code: &str) -> Vec<u8> {
    let Ok(mut mac) = HmacSha256::new_from_slice(pepper) else {
        unreachable!("HMAC-SHA256 accepts keys of any length");
    };
    mac.update(normalised_code.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Constant-time equality for HMAC/session-hash comparisons.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

// ---------------------------------------------------------------------
// Session tokens
// ---------------------------------------------------------------------

const SESSION_TOKEN_BYTES: usize = 32;

/// Generates a fresh 32-byte session token and its SHA-256 hash (the only
/// thing ever stored). Returns `(hex_token_for_cookie, hash_for_db)`.
pub fn generate_session_token() -> (String, Vec<u8>) {
    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    let hash = Sha256::digest(bytes).to_vec();
    (hex_encode(&bytes), hash)
}

/// Hashes a hex-decoded session token from a cookie for DB lookup.
pub fn hash_token(raw: &[u8]) -> Vec<u8> {
    Sha256::digest(raw).to_vec()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

// ---------------------------------------------------------------------
// Cookies
// ---------------------------------------------------------------------

const USER_COOKIE_BASE: &str = "mp_user";
const ADMIN_COOKIE_BASE: &str = "mp_admin";

pub const USER_SESSION_LIFETIME_SECS: i64 = 90 * 24 * 3600;
pub const ADMIN_SESSION_ABSOLUTE_SECS: i64 = 8 * 3600;
pub const ADMIN_SESSION_IDLE_SECS: i64 = 2 * 3600;
/// Sessions are only touched (last_seen_at slid forward) this often, to
/// keep SD-card writes low.
pub const SESSION_TOUCH_INTERVAL_SECS: i64 = 10 * 60;

fn cookie_name(base: &str, dev_insecure: bool) -> String {
    if dev_insecure {
        base.to_string()
    } else {
        format!("__Host-{base}")
    }
}

pub fn user_cookie_name(dev_insecure: bool) -> String {
    cookie_name(USER_COOKIE_BASE, dev_insecure)
}

pub fn admin_cookie_name(dev_insecure: bool) -> String {
    cookie_name(ADMIN_COOKIE_BASE, dev_insecure)
}

/// Builds a `Set-Cookie` header value by hand rather than through a typed
/// cookie builder: the `cookie`/`time` crates that provide one are only
/// transitive dependencies here (via `axum-extra`), and plain integer
/// `Max-Age` seconds is all RFC 6265 needs, so there's no need to name
/// those crates directly just to express a duration.
fn set_cookie_header(name: &str, value: &str, max_age_secs: i64, secure: bool) -> HeaderValue {
    let mut s =
        format!("{name}={value}; Path=/; Max-Age={max_age_secs}; HttpOnly; SameSite=Strict");
    if secure {
        s.push_str("; Secure");
    }
    HeaderValue::from_str(&s).unwrap_or_else(|_| HeaderValue::from_static("invalid=cookie"))
}

pub fn build_session_cookie(
    base: &str,
    token: &str,
    max_age_secs: i64,
    dev_insecure: bool,
) -> HeaderValue {
    let name = cookie_name(base, dev_insecure);
    set_cookie_header(&name, token, max_age_secs, !dev_insecure)
}

pub fn build_clear_cookie(base: &str, dev_insecure: bool) -> HeaderValue {
    let name = cookie_name(base, dev_insecure);
    set_cookie_header(&name, "", 0, !dev_insecure)
}

fn cookie_token(jar: &CookieJar, name: &str) -> Option<Vec<u8>> {
    hex_decode(jar.get(name)?.value())
}

// ---------------------------------------------------------------------
// Admin password hashing
// ---------------------------------------------------------------------

/// Argon2id, 19 MiB, 2 iterations, 1 lane, per the spec. `const fn` so an
/// invalid literal would fail the build, not a live request.
fn argon2_params() -> Params {
    const PARAMS: Result<Params, argon2::Error> = Params::new(19 * 1024, 2, 1, None);
    match PARAMS {
        Ok(p) => p,
        Err(_) => unreachable!("argon2 params are fixed, known-good constants"),
    }
}

fn argon2() -> Argon2<'static> {
    Argon2::new(Algorithm::Argon2id, Version::V0x13, argon2_params())
}

/// A real PHC hash of a fixed, never-used password, computed once with the
/// same cost parameters as real admin hashes. Verifying against this for
/// unknown usernames keeps login timing independent of whether the
/// username exists.
fn dummy_password_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        argon2()
            .hash_password(b"dummy-password-never-used-for-login")
            .map(|h| h.to_string())
            // Hashing a fixed byte string with fixed params cannot fail; if
            // it somehow did, falling back to a syntactically-invalid hash
            // still makes verification fail closed (never "succeeds").
            .unwrap_or_else(|_| "$argon2id$v=19$m=19456,t=2,p=1$invalid$invalid".to_string())
    })
}

/// Verifies a password against a PHC hash on the blocking pool. Always
/// takes the same code path (and roughly the same time) whether the hash
/// came from a real admin or [`dummy_password_hash`], which the caller
/// supplies for unknown usernames.
pub async fn verify_admin_password(password_hash: &str, password: String) -> bool {
    let password_hash = password_hash.to_string();
    tokio::task::spawn_blocking(move || {
        let Ok(parsed) = PasswordHash::new(&password_hash) else {
            return false;
        };
        argon2()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

/// Hashes a password for storage as `ADMIN_PASSWORD_HASH`. Used by the
/// `hash-password` CLI command, never by request handlers.
pub fn hash_admin_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    Ok(argon2().hash_password(password.as_bytes())?.to_string())
}

pub fn admin_password_hash_for(username_matches: bool, configured_hash: &str) -> String {
    if username_matches {
        configured_hash.to_string()
    } else {
        dummy_password_hash().to_string()
    }
}

// ---------------------------------------------------------------------
// Extractors
// ---------------------------------------------------------------------

/// A valid user session, with the member it belongs to.
pub struct RequireUser {
    pub member_id: i64,
    pub member_name: String,
    pub session_id: i64,
}

impl FromRequestParts<AppState> for RequireUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let jar = CookieJar::from_headers(&parts.headers);
        let name = user_cookie_name(state.config.cookie_dev_insecure);
        let raw_token = cookie_token(&jar, &name).ok_or(AppError::Unauthorized)?;
        let token_hash = hash_token(&raw_token);

        let session = db::find_user_session(&state.pool, &token_hash)
            .await?
            .ok_or(AppError::Unauthorized)?;

        let now = db::now_unix();
        if now >= session.expires_at {
            db::delete_session(&state.pool, session.session_id).await?;
            return Err(AppError::Unauthorized);
        }

        if now - session.last_seen_at >= SESSION_TOUCH_INTERVAL_SECS {
            let new_expires_at = now + USER_SESSION_LIFETIME_SECS;
            db::touch_session(
                &state.pool,
                session.session_id,
                Some(session.member_id),
                now,
                Some(new_expires_at),
            )
            .await?;
        }

        Ok(RequireUser {
            member_id: session.member_id,
            member_name: session.member_name,
            session_id: session.session_id,
        })
    }
}

/// A valid admin session.
pub struct RequireAdmin {
    pub session_id: i64,
}

impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let jar = CookieJar::from_headers(&parts.headers);
        let name = admin_cookie_name(state.config.cookie_dev_insecure);
        let raw_token = cookie_token(&jar, &name).ok_or(AppError::Unauthorized)?;
        let token_hash = hash_token(&raw_token);

        let session = db::find_admin_session(&state.pool, &token_hash)
            .await?
            .ok_or(AppError::Unauthorized)?;

        let now = db::now_unix();
        let idle_expired = now - session.last_seen_at >= ADMIN_SESSION_IDLE_SECS;
        let absolute_expired = now >= session.expires_at;
        if idle_expired || absolute_expired {
            db::delete_session(&state.pool, session.session_id).await?;
            return Err(AppError::Unauthorized);
        }

        if now - session.last_seen_at >= SESSION_TOUCH_INTERVAL_SECS {
            db::touch_session(&state.pool, session.session_id, None, now, None).await?;
        }

        Ok(RequireAdmin {
            session_id: session.session_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_lowercases_and_strips_separators() {
        assert_eq!(normalise_code("abcd-efgh"), "ABCDEFGH");
        assert_eq!(normalise_code("ABCD EFGH"), "ABCDEFGH");
        assert_eq!(normalise_code("ab cd-ef-gh"), "ABCDEFGH");
    }

    #[test]
    fn lookalike_characters_never_appear_in_generated_codes() {
        let code = generate_code(200);
        for excluded in ['0', 'O', '1', 'I', 'L'] {
            assert!(
                !code.contains(excluded),
                "code contained look-alike {excluded}"
            );
        }
    }

    #[test]
    fn generated_code_has_requested_length() {
        assert_eq!(generate_code(8).len(), 8);
        assert_eq!(generate_code(12).len(), 12);
    }

    #[test]
    fn display_format_inserts_single_dash() {
        assert_eq!(format_code_for_display("ABCDEFGH"), "ABCD-EFGH");
    }

    #[test]
    fn hmac_is_deterministic_and_key_dependent() {
        let a = hmac_code(b"pepper-one-pepper-one-pepper-one", "ABCDEFGH");
        let b = hmac_code(b"pepper-one-pepper-one-pepper-one", "ABCDEFGH");
        let c = hmac_code(b"pepper-two-pepper-two-pepper-two", "ABCDEFGH");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 255, 16, 128];
        let s = hex_encode(&bytes);
        assert_eq!(hex_decode(&s).unwrap(), bytes);
    }

    #[test]
    fn hex_decode_rejects_garbage() {
        assert!(hex_decode("not-hex").is_none());
        assert!(hex_decode("abc").is_none()); // odd length
    }

    #[test]
    fn session_token_hash_matches_hash_token() {
        let (hex_token, stored_hash) = generate_session_token();
        let raw = hex_decode(&hex_token).unwrap();
        assert_eq!(hash_token(&raw), stored_hash);
    }

    #[test]
    fn cookie_names_carry_host_prefix_unless_dev() {
        assert_eq!(user_cookie_name(false), "__Host-mp_user");
        assert_eq!(admin_cookie_name(false), "__Host-mp_admin");
        assert_eq!(user_cookie_name(true), "mp_user");
    }

    #[test]
    fn session_cookie_has_required_flags() {
        let header = build_session_cookie("mp_user", "tok", 3600, false);
        let s = header.to_str().unwrap();
        assert!(s.starts_with("__Host-mp_user=tok;"));
        assert!(s.contains("Path=/"));
        assert!(s.contains("HttpOnly"));
        assert!(s.contains("Secure"));
        assert!(s.contains("SameSite=Strict"));
        assert!(!s.contains("Domain="));
    }

    #[test]
    fn dev_cookie_drops_host_prefix_and_secure() {
        let header = build_session_cookie("mp_user", "tok", 3600, true);
        let s = header.to_str().unwrap();
        assert!(s.starts_with("mp_user=tok;"));
        assert!(!s.contains("__Host-"));
        assert!(!s.contains("Secure"));
    }

    #[test]
    fn clear_cookie_has_zero_max_age() {
        let header = build_clear_cookie("mp_user", false);
        let s = header.to_str().unwrap();
        assert!(s.contains("Max-Age=0"));
    }

    #[tokio::test]
    async fn verify_admin_password_accepts_correct_password() {
        let hash = hash_admin_password("correct horse battery staple").unwrap();
        assert!(verify_admin_password(&hash, "correct horse battery staple".to_string()).await);
    }

    #[tokio::test]
    async fn verify_admin_password_rejects_wrong_password() {
        let hash = hash_admin_password("correct horse battery staple").unwrap();
        assert!(!verify_admin_password(&hash, "wrong password".to_string()).await);
    }

    #[tokio::test]
    async fn dummy_hash_is_well_formed_but_never_guessable() {
        // It's a real, working Argon2id hash (so verification against it
        // costs the same as a real one)...
        assert!(
            verify_admin_password(
                dummy_password_hash(),
                "dummy-password-never-used-for-login".to_string()
            )
            .await
        );
        // ...of a fixed internal string no login attempt will ever send.
        assert!(!verify_admin_password(dummy_password_hash(), "anything".to_string()).await);
    }
}
