//! Redis-backed signed audit log.
//!
//! Layout: `referee:audit:{swap_id}` is a Redis LIST holding
//! JSON-encoded [`AuditEntry`] entries in append order. Reading the
//! full chain is `LRANGE 0 -1`. Tip is the last element.

use async_trait::async_trait;
use redis::AsyncCommands;

use crate::audit::{AuditEntry, AuditLog};
use crate::error::{RefereeError, Result};
use crate::sign::Identity;
use crate::state::{tag_for_phase, SwapId};

/// Key prefix for per-swap audit lists.
pub const AUDIT_KEY_PREFIX: &str = "referee:audit:";

fn audit_key(id: &SwapId) -> String {
    format!("{AUDIT_KEY_PREFIX}{}", id.0)
}

/// Redis-backed `AuditLog`.
pub struct RedisAuditLog {
    conn: tokio::sync::Mutex<redis::aio::ConnectionManager>,
}

impl RedisAuditLog {
    /// Open a connection to the Redis URL.
    pub async fn new(url: &str) -> Result<Self> {
        let client = redis::Client::open(url)
            .map_err(|e| RefereeError::Store(format!("redis client: {e}")))?;
        let conn = redis::aio::ConnectionManager::new(client)
            .await
            .map_err(|e| RefereeError::Store(format!("redis connect: {e}")))?;
        Ok(Self {
            conn: tokio::sync::Mutex::new(conn),
        })
    }
}

#[async_trait]
impl AuditLog for RedisAuditLog {
    async fn append(&self, identity: &Identity, entry: &mut AuditEntry) -> Result<String> {
        let canonical = entry.canonical_body();
        let tag = tag_for_phase(&entry.phase);
        entry.signature = identity.sign(tag, &canonical);
        let tip = entry.tip_hash();
        let json = serde_json::to_string(entry)
            .map_err(|e| RefereeError::Store(format!("encode: {e}")))?;
        let mut conn = self.conn.lock().await;
        let _: () = conn
            .rpush(audit_key(&entry.swap_id), json)
            .await
            .map_err(|e| RefereeError::Store(format!("redis rpush: {e}")))?;
        Ok(tip)
    }

    async fn entries_for(&self, swap_id: &SwapId) -> Result<Vec<AuditEntry>> {
        let mut conn = self.conn.lock().await;
        let items: Vec<String> = conn
            .lrange(audit_key(swap_id), 0, -1)
            .await
            .map_err(|e| RefereeError::Store(format!("redis lrange: {e}")))?;
        let mut out = Vec::with_capacity(items.len());
        for s in items {
            let e: AuditEntry = serde_json::from_str(&s)
                .map_err(|e| RefereeError::Store(format!("decode: {e}")))?;
            out.push(e);
        }
        Ok(out)
    }

    async fn tip_for(&self, swap_id: &SwapId) -> Result<String> {
        let mut conn = self.conn.lock().await;
        let last: Option<String> = conn
            .lindex(audit_key(swap_id), -1)
            .await
            .map_err(|e| RefereeError::Store(format!("redis lindex: {e}")))?;
        match last {
            None => Ok(String::new()),
            Some(s) => {
                let e: AuditEntry = serde_json::from_str(&s)
                    .map_err(|e| RefereeError::Store(format!("decode: {e}")))?;
                Ok(e.tip_hash())
            }
        }
    }
}
