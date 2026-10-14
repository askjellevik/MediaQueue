//! Optional Cloudflare Access check in front of the admin area (cargo
//! feature `cf-access`, switched on with `CF_ACCESS_ENABLED=1`).
//!
//! Requests for `/admin` and `/api/admin/*` must carry a valid
//! `Cf-Access-Jwt-Assertion`: RS256, signed by one of the team's published
//! keys, for our AUD and issuer, and not expired. The admin password login
//! still applies on top. The signing keys are cached for an hour; an
//! unknown key ID forces a refresh at most once a minute, so a stream of
//! forged tokens cannot turn into a stream of fetches.

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use moka::future::Cache;
use serde::Deserialize;

use crate::{clock::Clock, config::CfAccessConfig, error::AppError, state::AppState};

pub const HEADER: &str = "cf-access-jwt-assertion";
const KEYS_TTL: Duration = Duration::from_secs(3600);
const MIN_FORCED_REFRESH: Duration = Duration::from_secs(60);

type Keys = Arc<HashMap<String, DecodingKey>>;

pub struct CfAccess {
    http: reqwest::Client,
    certs_url: String,
    validation: Validation,
    keys: Cache<(), Keys>,
    clock: Clock,
    /// Clock-elapsed milliseconds of the last refresh forced by an
    /// unknown key ID.
    last_forced_ms: AtomicU64,
}

/// The claims themselves are checked by `Validation`; nothing else in the
/// token is used.
#[derive(Deserialize)]
struct Claims {}

impl CfAccess {
    pub fn new(http: reqwest::Client, config: &CfAccessConfig, clock: Clock) -> Self {
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&config.audience]);
        validation.set_issuer(&[&config.issuer]);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        Self {
            http,
            certs_url: config.certs_url.clone(),
            validation,
            keys: Cache::builder()
                .max_capacity(1)
                .time_to_live(KEYS_TTL)
                .build(),
            clock,
            last_forced_ms: AtomicU64::new(0),
        }
    }

    /// The team's keys by key ID; concurrent callers share one fetch.
    async fn keys(&self) -> Option<Keys> {
        self.keys
            .try_get_with((), self.fetch_keys())
            .await
            .map_err(|error| tracing::warn!(%error, "fetching Cloudflare Access keys failed"))
            .ok()
    }

    async fn fetch_keys(&self) -> Result<Keys, reqwest::Error> {
        let set: JwkSet = self
            .http
            .get(&self.certs_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let keys = set
            .keys
            .iter()
            .filter_map(|jwk| {
                let kid = jwk.common.key_id.clone()?;
                DecodingKey::from_jwk(jwk).ok().map(|key| (kid, key))
            })
            .collect();
        Ok(Arc::new(keys))
    }

    /// Whether an unknown key ID may trigger a refetch now.
    fn may_force_refresh(&self) -> bool {
        let now = u64::try_from(self.clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.last_forced_ms.load(Ordering::Acquire);
        let due = last == 0 || now.saturating_sub(last) >= MIN_FORCED_REFRESH.as_millis() as u64;
        due && self
            .last_forced_ms
            .compare_exchange(last, now.max(1), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub async fn verify(&self, token: &str) -> bool {
        let Ok(header) = decode_header(token) else {
            return false;
        };
        let Some(kid) = header.kid.filter(|_| header.alg == Algorithm::RS256) else {
            return false;
        };
        let Some(mut keys) = self.keys().await else {
            return false;
        };
        if !keys.contains_key(&kid) && self.may_force_refresh() {
            self.keys.invalidate(&()).await;
            match self.keys().await {
                Some(fresh) => keys = fresh,
                None => return false,
            }
        }
        let Some(key) = keys.get(&kid) else {
            return false;
        };
        decode::<Claims>(token, key, &self.validation).is_ok()
    }
}

/// The admin area: the page (by any path that serves it) and its API.
fn is_protected(path: &str) -> bool {
    path.starts_with("/api/admin") || path == "/admin" || path.ends_with("/admin.html")
}

/// Middleware: a no-op unless Access is configured; then the admin area
/// needs a valid assertion, or the answer is 403.
pub async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(cf) = state.cf_access.as_ref() else {
        return next.run(req).await;
    };
    if !is_protected(req.uri().path()) {
        return next.run(req).await;
    }
    let token = req
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if token.is_empty() || !cf.verify(token).await {
        tracing::warn!(
            path = req.uri().path(),
            "Cloudflare Access assertion missing or invalid"
        );
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_every_way_to_the_admin_area() {
        for path in [
            "/admin",
            "/admin.html",
            "/v/abc/admin.html",
            "/api/admin/login",
            "/api/admin/members",
        ] {
            assert!(is_protected(path), "{path}");
        }
        for path in [
            "/",
            "/api/me",
            "/api/search",
            "/v/abc/js/admin.js",
            "/healthz",
        ] {
            assert!(!is_protected(path), "{path}");
        }
    }
}
