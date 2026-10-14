//! `AppState` is the one piece of shared state cloned into every handler.
//! Later phases add the reqwest client, TMDB caches and rate limiters here.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::Semaphore;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Arc<Config>,
    /// Bounds concurrent Argon2id hashing to 1 permit, so a flood of admin
    /// logins queues instead of exhausting RAM on 19 MiB hashes at once.
    pub admin_login_semaphore: Arc<Semaphore>,
}

impl AppState {
    pub fn new(pool: SqlitePool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            admin_login_semaphore: Arc::new(Semaphore::new(1)),
        }
    }
}
