//! TMDB client with the epoch caches from docs/SPEC.md "TMDB caching and
//! speed".
//!
//! Each cache is keyed by `(key, epoch)` with `epoch = clock.now / period`.
//! A lookup is in one of three states:
//!
//! - fresh: an entry for the current epoch exists; no upstream call.
//! - stale-usable: an entry from one of the previous few epochs is still
//!   younger than the hard limit; refresh it, and if TMDB is down serve the
//!   stale entry and stop calling TMDB for [`UNHEALTHY_FOR`].
//! - gone: nothing young enough; TMDB must answer or the caller gets 503.
//!
//! Freshness is decided only by the injectable [`Clock`]; moka's own
//! `time_to_live` just frees memory a little after an entry stops being
//! usable.

use std::{
    future::Future,
    hash::Hash,
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use governor::{
    Quota, RateLimiter,
    middleware::NoOpMiddleware,
    nanos::Nanos,
    state::{InMemoryState, NotKeyed},
};
use moka::future::Cache;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{clock::Clock, config::Config};

/// How long TMDB counts as down after a failed refresh. Stale entries are
/// served without trying upstream during this window.
const UNHEALTHY_FOR: Duration = Duration::from_secs(30);

const SEARCH_CAPACITY: u64 = 500;
const SEARCH_EPOCH_SECS: u64 = 600;
const SEARCH_MAX_AGE_SECS: u64 = 3600;
const SEARCH_LOOKBACK_EPOCHS: u64 = 6;
const SEARCH_TTL: Duration = Duration::from_secs(65 * 60);

const DETAIL_CAPACITY: u64 = 500;
const DETAIL_EPOCH_SECS: u64 = 86_400;
const DETAIL_MAX_AGE_SECS: u64 = 7 * 86_400;
const DETAIL_LOOKBACK_EPOCHS: u64 = 7;
const DETAIL_TTL: Duration = Duration::from_secs(7 * 86_400 + 3600);

/// Results per search sent to the browser.
pub const MAX_RESULTS: usize = 8;

/// Overviews are cut to this many characters to keep the payload small;
/// the confirm sheet only shows a trimmed overview anyway.
const MAX_OVERVIEW_CHARS: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Movie,
    Tv,
}

impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaType::Movie => "movie",
            MediaType::Tv => "tv",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "movie" => Some(MediaType::Movie),
            "tv" => Some(MediaType::Tv),
            _ => None,
        }
    }
}

/// The `type` filter of `/api/search`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchType {
    #[default]
    All,
    Movie,
    Tv,
}

impl SearchType {
    fn allows(self, media_type: MediaType) -> bool {
        match self {
            SearchType::All => true,
            SearchType::Movie => media_type == MediaType::Movie,
            SearchType::Tv => media_type == MediaType::Tv,
        }
    }
}

/// One slimmed search result, as cached. The per-user `state` is added by
/// the route, never cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchItem {
    pub tmdb_id: u32,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i32>,
    pub poster_path: Option<String>,
    pub overview: String,
}

/// What a request row needs from TMDB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub title: String,
    pub year: Option<i32>,
    pub poster_path: Option<String>,
    pub imdb_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TmdbError {
    /// Network error, timeout, 5xx, 429 or an unreadable body.
    #[error("tmdb unavailable")]
    Unavailable,
    /// Our own global upstream limit is used up. Treated like
    /// `Unavailable`, except it says nothing about TMDB's health.
    #[error("tmdb upstream limit reached")]
    LocalLimit,
    /// Any other 4xx. Never answered from stale data.
    #[error("not found at tmdb")]
    NotFound,
}

/// Normalises a search query for the cache key and the upstream call:
/// lowercase, trimmed, inner whitespace collapsed to single spaces.
pub fn normalise_query(q: &str) -> String {
    q.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

struct Cached<V> {
    value: V,
    fetched_at: u64,
}

/// A moka cache keyed by `(key, epoch)`, plus the rules for which older
/// epochs may still be served.
struct EpochCache<K, V> {
    cache: Cache<(K, u64), Arc<Cached<V>>>,
    epoch_secs: u64,
    lookback: u64,
    max_age_secs: u64,
}

impl<K, V> EpochCache<K, V>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn new(capacity: u64, ttl: Duration, epoch_secs: u64, lookback: u64, max_age: u64) -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(capacity)
                .time_to_live(ttl)
                .build(),
            epoch_secs,
            lookback,
            max_age_secs: max_age,
        }
    }

    /// The newest entry from a previous epoch that is still young enough
    /// to serve, with its epoch.
    async fn find_stale(&self, key: &K, epoch: u64, now: u64) -> Option<(u64, Arc<Cached<V>>)> {
        for back in 1..=self.lookback {
            let Some(old_epoch) = epoch.checked_sub(back) else {
                break;
            };
            if let Some(entry) = self.cache.get(&(key.clone(), old_epoch)).await
                && now.saturating_sub(entry.fetched_at) < self.max_age_secs
            {
                return Some((old_epoch, entry));
            }
        }
        None
    }

    async fn remove_older(&self, key: &K, epoch: u64) {
        for back in 1..=self.lookback {
            let Some(old_epoch) = epoch.checked_sub(back) else {
                break;
            };
            self.cache.invalidate(&(key.clone(), old_epoch)).await;
        }
    }
}

type UpstreamLimiter = RateLimiter<NotKeyed, InMemoryState, Clock, NoOpMiddleware<Nanos>>;

pub struct Tmdb {
    http: reqwest::Client,
    base_url: String,
    token: String,
    clock: Clock,
    limiter: UpstreamLimiter,
    /// Clock-elapsed milliseconds until which TMDB counts as down.
    unhealthy_until_ms: AtomicU64,
    search_cache: EpochCache<(String, SearchType), Arc<Vec<SearchItem>>>,
    detail_cache: EpochCache<(u32, MediaType), Detail>,
}

impl Tmdb {
    pub fn new(http: reqwest::Client, config: &Config, clock: Clock) -> Self {
        let per_second =
            NonZeroU32::new(config.rate_limits.tmdb_per_second).unwrap_or(NonZeroU32::MIN);
        Self {
            http,
            base_url: config.tmdb_base_url.clone(),
            token: config.tmdb_token.clone(),
            limiter: RateLimiter::direct_with_clock(Quota::per_second(per_second), clock.clone()),
            clock,
            unhealthy_until_ms: AtomicU64::new(0),
            search_cache: EpochCache::new(
                SEARCH_CAPACITY,
                SEARCH_TTL,
                SEARCH_EPOCH_SECS,
                SEARCH_LOOKBACK_EPOCHS,
                SEARCH_MAX_AGE_SECS,
            ),
            detail_cache: EpochCache::new(
                DETAIL_CAPACITY,
                DETAIL_TTL,
                DETAIL_EPOCH_SECS,
                DETAIL_LOOKBACK_EPOCHS,
                DETAIL_MAX_AGE_SECS,
            ),
        }
    }

    /// Up to [`MAX_RESULTS`] movie and TV results for an already
    /// normalised query (see [`normalise_query`]).
    pub async fn search(
        &self,
        query: &str,
        search_type: SearchType,
    ) -> Result<Arc<Vec<SearchItem>>, TmdbError> {
        let key = (query.to_string(), search_type);
        self.cached(&self.search_cache, key, || async move {
            let response: MultiResponse = self
                .get_json(
                    "/search/multi",
                    &[("query", query), ("include_adult", "false"), ("page", "1")],
                )
                .await?;
            Ok(Arc::new(slim_results(response, search_type)))
        })
        .await
    }

    /// Title, year, poster and IMDb ID for one title, for creating a
    /// request. Never called during search.
    pub async fn detail(&self, tmdb_id: u32, media_type: MediaType) -> Result<Detail, TmdbError> {
        self.cached(&self.detail_cache, (tmdb_id, media_type), || async move {
            match media_type {
                MediaType::Movie => {
                    let movie: MovieDetail =
                        self.get_json(&format!("/movie/{tmdb_id}"), &[]).await?;
                    Ok(Detail {
                        title: movie.title,
                        year: year_of(movie.release_date.as_deref()),
                        poster_path: safe_poster_path(movie.poster_path),
                        imdb_id: safe_imdb_id(movie.imdb_id),
                    })
                }
                MediaType::Tv => {
                    // One call instead of two: `append_to_response` embeds
                    // the `/tv/{id}/external_ids` payload.
                    let tv: TvDetail = self
                        .get_json(
                            &format!("/tv/{tmdb_id}"),
                            &[("append_to_response", "external_ids")],
                        )
                        .await?;
                    Ok(Detail {
                        title: tv.name,
                        year: year_of(tv.first_air_date.as_deref()),
                        poster_path: safe_poster_path(tv.poster_path),
                        imdb_id: safe_imdb_id(tv.external_ids.and_then(|e| e.imdb_id)),
                    })
                }
            }
        })
        .await
    }

    /// The three-state lookup shared by both caches.
    async fn cached<K, V, F, Fut>(
        &self,
        cache: &EpochCache<K, V>,
        key: K,
        fetch: F,
    ) -> Result<V, TmdbError>
    where
        K: Hash + Eq + Clone + Send + Sync + 'static,
        V: Clone + Send + Sync + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, TmdbError>>,
    {
        let now = self.clock.unix_secs();
        let epoch = now / cache.epoch_secs;
        let current = (key.clone(), epoch);

        if let Some(fresh) = cache.cache.get(&current).await {
            return Ok(fresh.value.clone());
        }

        let stale = cache.find_stale(&key, epoch, now).await;
        if let Some((_, entry)) = &stale
            && self.is_unhealthy()
        {
            return Ok(entry.value.clone());
        }

        // Concurrent callers for the same key share this one load; an
        // `Err` is handed to all of them and not cached.
        let loaded = cache
            .cache
            .try_get_with(current, async move {
                let value = fetch().await?;
                Ok(Arc::new(Cached {
                    value,
                    fetched_at: self.clock.unix_secs(),
                }))
            })
            .await;

        match loaded {
            Ok(entry) => {
                if stale.is_some() {
                    cache.remove_older(&key, epoch).await;
                }
                Ok(entry.value.clone())
            }
            Err(error) => match *error {
                TmdbError::Unavailable | TmdbError::LocalLimit => {
                    if *error == TmdbError::Unavailable {
                        self.mark_unhealthy();
                    }
                    match stale {
                        Some((_, entry)) => {
                            tracing::warn!("tmdb refresh failed, serving stale entry");
                            Ok(entry.value.clone())
                        }
                        None => Err(*error),
                    }
                }
                TmdbError::NotFound => {
                    cache.remove_older(&key, epoch).await;
                    Err(TmdbError::NotFound)
                }
            },
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn is_unhealthy(&self) -> bool {
        self.now_ms() < self.unhealthy_until_ms.load(Ordering::Acquire)
    }

    fn mark_unhealthy(&self) {
        let until = self
            .now_ms()
            .saturating_add(UNHEALTHY_FOR.as_millis() as u64);
        self.unhealthy_until_ms.store(until, Ordering::Release);
    }

    /// One GET against TMDB, retried once on a connection error only.
    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, TmdbError> {
        if self.limiter.check().is_err() {
            tracing::warn!("tmdb upstream rate limit reached");
            return Err(TmdbError::LocalLimit);
        }
        let url = format!("{}{}", self.base_url, path);

        let mut retried = false;
        let response = loop {
            let sent = self
                .http
                .get(&url)
                .bearer_auth(&self.token)
                .query(query)
                .send()
                .await;
            match sent {
                Ok(response) => break response,
                Err(error) if error.is_connect() && !retried => retried = true,
                Err(error) => {
                    // The URL holds the search text; leave it out of logs.
                    tracing::warn!(error = %error.without_url(), "tmdb request failed");
                    return Err(TmdbError::Unavailable);
                }
            }
        };

        let status = response.status();
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            tracing::warn!(status = status.as_u16(), "tmdb unavailable");
            return Err(TmdbError::Unavailable);
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            tracing::error!(
                status = status.as_u16(),
                "tmdb rejected the token; check TMDB_TOKEN"
            );
            return Err(TmdbError::NotFound);
        }
        if status.is_client_error() {
            return Err(TmdbError::NotFound);
        }
        if !status.is_success() {
            return Err(TmdbError::Unavailable);
        }
        response.json::<T>().await.map_err(|error| {
            tracing::warn!(error = %error.without_url(), "tmdb sent an unreadable body");
            TmdbError::Unavailable
        })
    }
}

// ---------------------------------------------------------------------
// TMDB payloads and slimming
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct MultiResponse {
    #[serde(default)]
    results: Vec<MultiItem>,
}

#[derive(Clone, Deserialize)]
struct MultiItem {
    id: u32,
    #[serde(default)]
    media_type: String,
    title: Option<String>,
    name: Option<String>,
    release_date: Option<String>,
    first_air_date: Option<String>,
    poster_path: Option<String>,
    overview: Option<String>,
}

#[derive(Deserialize)]
struct MovieDetail {
    title: String,
    release_date: Option<String>,
    poster_path: Option<String>,
    imdb_id: Option<String>,
}

#[derive(Deserialize)]
struct TvDetail {
    name: String,
    first_air_date: Option<String>,
    poster_path: Option<String>,
    external_ids: Option<ExternalIds>,
}

#[derive(Deserialize)]
struct ExternalIds {
    imdb_id: Option<String>,
}

/// Keeps movies and series (dropping people and anything else), applies
/// the type filter and cuts to [`MAX_RESULTS`].
fn slim_results(response: MultiResponse, search_type: SearchType) -> Vec<SearchItem> {
    response
        .results
        .into_iter()
        .filter_map(|item| {
            let media_type = MediaType::parse(&item.media_type)?;
            if !search_type.allows(media_type) || item.id == 0 {
                return None;
            }
            let (title, date) = match media_type {
                MediaType::Movie => (item.title, item.release_date),
                MediaType::Tv => (item.name, item.first_air_date),
            };
            let title = title.filter(|t| !t.trim().is_empty())?;
            Some(SearchItem {
                tmdb_id: item.id,
                media_type,
                title,
                year: year_of(date.as_deref()),
                poster_path: safe_poster_path(item.poster_path),
                overview: truncate_chars(
                    item.overview.as_deref().unwrap_or(""),
                    MAX_OVERVIEW_CHARS,
                ),
            })
        })
        .take(MAX_RESULTS)
        .collect()
}

/// `"2021-09-15"` -> `Some(2021)`; empty or malformed -> `None`.
fn year_of(date: Option<&str>) -> Option<i32> {
    date?.get(..4)?.parse().ok()
}

/// TMDB poster paths look like `/abc123.jpg`. Anything else is dropped,
/// since the browser builds an image URL from it.
fn safe_poster_path(path: Option<String>) -> Option<String> {
    path.filter(|p| {
        p.len() > 1
            && p.len() <= 64
            && p.starts_with('/')
            && p[1..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    })
}

/// `tt` followed by digits, or nothing. The ID ends up in admin links.
fn safe_imdb_id(id: Option<String>) -> Option<String> {
    id.filter(|i| {
        i.len() > 2
            && i.len() <= 16
            && i.starts_with("tt")
            && i[2..].bytes().all(|b| b.is_ascii_digit())
    })
}

fn truncate_chars(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((cut, _)) => format!("{}…", s[..cut].trim_end()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_query_lowercases_trims_and_collapses() {
        assert_eq!(normalise_query("  Dune   Part\tTwo "), "dune part two");
        assert_eq!(normalise_query("ÅSA"), "åsa");
    }

    #[test]
    fn year_of_parses_dates() {
        assert_eq!(year_of(Some("2021-09-15")), Some(2021));
        assert_eq!(year_of(Some("")), None);
        assert_eq!(year_of(Some("abc")), None);
        assert_eq!(year_of(None), None);
    }

    #[test]
    fn poster_and_imdb_ids_are_validated() {
        assert_eq!(
            safe_poster_path(Some("/abc_1-2.jpg".into())).as_deref(),
            Some("/abc_1-2.jpg")
        );
        assert_eq!(safe_poster_path(Some("//evil.com/x.jpg".into())), None);
        assert_eq!(safe_poster_path(Some("/a b.jpg".into())), None);
        assert_eq!(safe_poster_path(Some("/".into())), None);
        assert_eq!(
            safe_imdb_id(Some("tt1160419".into())).as_deref(),
            Some("tt1160419")
        );
        assert_eq!(safe_imdb_id(Some("".into())), None);
        assert_eq!(safe_imdb_id(Some("tt12x".into())), None);
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate_chars("short", 10), "short");
        assert_eq!(truncate_chars("ææææææ", 3), "æææ…");
    }

    #[test]
    fn slim_drops_people_filters_type_and_caps_at_eight() {
        let mut results = vec![MultiItem {
            id: 1,
            media_type: "person".into(),
            title: None,
            name: Some("Someone".into()),
            release_date: None,
            first_air_date: None,
            poster_path: None,
            overview: None,
        }];
        for id in 2..20 {
            results.push(MultiItem {
                id,
                media_type: if id % 2 == 0 { "movie" } else { "tv" }.into(),
                title: Some(format!("Movie {id}")),
                name: Some(format!("Show {id}")),
                release_date: Some("2020-01-01".into()),
                first_air_date: Some("2019-01-01".into()),
                poster_path: Some("/p.jpg".into()),
                overview: Some("x".into()),
            });
        }
        let all = slim_results(
            MultiResponse {
                results: results.clone(),
            },
            SearchType::All,
        );
        assert_eq!(all.len(), MAX_RESULTS);
        assert!(all.iter().all(|i| i.tmdb_id != 1));
        assert_eq!(all[0].title, "Movie 2");
        assert_eq!(all[0].year, Some(2020));
        assert_eq!(all[1].title, "Show 3");
        assert_eq!(all[1].year, Some(2019));

        let tv = slim_results(MultiResponse { results }, SearchType::Tv);
        assert!(tv.iter().all(|i| i.media_type == MediaType::Tv));
    }
}
