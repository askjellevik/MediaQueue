//! Live updates: content-free "something changed" ticks over server-sent
//! events. Clients refetch the lists they already show, so a tick never
//! carries request data and an unauthorised reader learns nothing from it.

use std::{convert::Infallible, future::Future, sync::Arc, time::Duration};

use axum::response::sse::{Event, Sse};
use futures_util::{Stream, stream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch};

use crate::error::AppError;

/// Streams open at once, across members and admin. Each one is a few
/// hundred bytes of state, but file descriptors are not free on a Pi.
pub const MAX_STREAMS: usize = 200;
/// Between keep-alive comments. Also how often an open stream re-checks that
/// its session is still valid, and below the idle timeout of most proxies.
pub const HEARTBEAT: Duration = Duration::from_secs(25);
const CHANNEL_CAPACITY: usize = 64;

/// Who a tick is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tick {
    Members,
    Admin,
}

#[derive(Clone)]
pub struct Hub {
    ticks: broadcast::Sender<Tick>,
    permits: Arc<Semaphore>,
    shutdown: Arc<watch::Sender<bool>>,
    heartbeat: Duration,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        Self::with_limits(MAX_STREAMS, HEARTBEAT)
    }

    /// Smaller numbers let tests finish quickly.
    pub fn with_limits(max_streams: usize, heartbeat: Duration) -> Self {
        Self {
            ticks: broadcast::channel(CHANNEL_CAPACITY).0,
            permits: Arc::new(Semaphore::new(max_streams)),
            shutdown: Arc::new(watch::channel(false).0),
            heartbeat,
        }
    }

    /// Nobody listening is fine.
    pub fn publish(&self, tick: Tick) {
        let _ = self.ticks.send(tick);
    }

    /// Ends every open stream. Without this, graceful shutdown would wait
    /// for them forever.
    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Opens a stream of `audience` ticks. `still_valid` runs on every
    /// heartbeat; when it says no, the stream ends and the client's
    /// reconnect meets a 401 instead of a silent, endless stream.
    pub fn open<F, Fut>(
        &self,
        audience: Tick,
        still_valid: F,
    ) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>> + use<F, Fut>>, AppError>
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = bool> + Send,
    {
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::Unavailable)?;
        let mut heartbeat =
            tokio::time::interval_at(tokio::time::Instant::now() + self.heartbeat, self.heartbeat);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let state = Open {
            audience,
            ticks: self.ticks.subscribe(),
            shutdown: self.shutdown.subscribe(),
            heartbeat,
            still_valid,
            _permit: permit,
        };
        let stream = stream::unfold(state, |mut s| async move {
            if *s.shutdown.borrow() {
                return None;
            }
            loop {
                tokio::select! {
                    received = s.ticks.recv() => match received {
                        Ok(t) if t != s.audience => continue,
                        // Lagging means ticks were dropped: say so once, the
                        // client refetches anyway.
                        Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                            return Some((Ok(tick_event()), s));
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    },
                    _ = s.heartbeat.tick() => {
                        if !(s.still_valid)().await {
                            return None;
                        }
                        return Some((Ok(Event::default().comment("keep-alive")), s));
                    }
                    // Only ever set to true, so any change (or the hub going away) ends it.
                    _ = s.shutdown.changed() => return None,
                }
            }
        });
        Ok(Sse::new(stream))
    }
}

/// `data` is a placeholder: an event without data lines is never dispatched
/// by browsers.
fn tick_event() -> Event {
    Event::default().event("tick").data("1")
}

struct Open<F> {
    audience: Tick,
    ticks: broadcast::Receiver<Tick>,
    shutdown: watch::Receiver<bool>,
    heartbeat: tokio::time::Interval,
    still_valid: F,
    /// Released when the client goes away and the stream is dropped.
    _permit: OwnedSemaphorePermit,
}
