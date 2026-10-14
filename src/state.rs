//! `AppState` is the one piece of shared state cloned into every handler.
//! Later phases add the reqwest client, TMDB caches and rate limiters here.

use std::sync::Arc;

use sqlx::SqlitePool;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
        }
    }
}
