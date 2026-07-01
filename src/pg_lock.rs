//! PostgreSQL advisory-lock [`CoordinationLock`] backend (tokio-postgres).
//!
//! Uses session-level `pg_advisory_lock` / `pg_advisory_unlock` on a single
//! bigint key (derived from the `key` string via a stable hash). Suitable for
//! coordinating entelecheia / shittim-chest replicas that already share a
//! Postgres. The connection is supplied by the caller (connected via
//! `tokio_postgres::connect` in their runtime).

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::traits::{CoordinationLock, LockError, LockGuard};
use async_trait::async_trait;
use tokio_postgres::Client;

/// Postgres-backed coordination lock over a shared connection.
pub struct PgLock {
    client: Arc<Client>,
}

impl PgLock {
    /// Wrap an already-connected tokio-postgres [`Client`] (share it via `Arc`).
    #[must_use]
    pub fn new(client: Arc<Client>) -> Self {
        Self { client }
    }
}

struct PgGuard {
    client: Arc<Client>,
    key: i64,
}

#[async_trait]
impl LockGuard for PgGuard {
    async fn release(&mut self) {
        // Session-level advisory lock is freed by pg_advisory_unlock.
        let _ = self
            .client
            .execute("SELECT pg_advisory_unlock($1)", &[&self.key])
            .await;
    }
}

#[async_trait]
impl CoordinationLock for PgLock {
    async fn acquire(&self, key: &str, _lease: Duration) -> Result<Box<dyn LockGuard>, LockError> {
        let k = key_to_i64(key);
        let row = self
            .client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&k])
            .await
            .map_err(|e| LockError::Io(io::Error::other(format!("pg lock: {e}"))))?;
        let got: bool = row.get(0);
        if got {
            Ok(Box::new(PgGuard {
                client: Arc::clone(&self.client),
                key: k,
            }))
        } else {
            Err(LockError::Contended(format!(
                "pg advisory lock on '{key}' held by another live session"
            )))
        }
    }
}

/// Stable, deterministic string→i64 mapping (fnv-1a 64, reinterpreted as i64).
fn key_to_i64(key: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in key.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hash_is_stable() {
        assert_eq!(key_to_i64("entelecheia"), key_to_i64("entelecheia"));
        assert_ne!(key_to_i64("a"), key_to_i64("b"));
    }
}
