//! Redis-backed swap-state store.
//!
//! Keys:
//! - `referee:swap:{id}` → JSON-encoded [`Transaction`].
//! - `referee:by-bob:{fp}` (sorted set, score = `created_at_unix`,
//!   member = `swap_id`) — index for `list_by_party` on the Bob side.
//! - `referee:by-alice:{fp}` — symmetric Alice-side index.
//!
//! Reads / writes use a Lua script (`SCRIPT_UPSERT`) so the row + the
//! two index entries land atomically; concurrent upserts cannot leave
//! the index lagging the row.
//!
//! Connection multiplexing via `redis::aio::ConnectionManager`. The
//! manager auto-reconnects on disconnect; transient outages surface as
//! `RefereeError::Store`.

use async_trait::async_trait;
use redis::AsyncCommands;

use crate::error::{RefereeError, Result};
use crate::state::{PgpFingerprint, SwapId};
use crate::store::SwapStore;
use crate::transaction::{PartyRole, Transaction, TransactionSummary};

/// Key prefix for swap rows.
pub const SWAP_KEY_PREFIX: &str = "referee:swap:";
const BY_BOB_PREFIX: &str = "referee:by-bob:";
const BY_ALICE_PREFIX: &str = "referee:by-alice:";

fn swap_key(id: &SwapId) -> String {
    format!("{SWAP_KEY_PREFIX}{}", id.0)
}

fn by_bob_key(fp: &PgpFingerprint) -> String {
    format!("{BY_BOB_PREFIX}{}", fp.0)
}

fn by_alice_key(fp: &PgpFingerprint) -> String {
    format!("{BY_ALICE_PREFIX}{}", fp.0)
}

/// Redis-backed `SwapStore`. Construct via [`RedisSwapStore::new`].
pub struct RedisSwapStore {
    conn: tokio::sync::Mutex<redis::aio::ConnectionManager>,
}

impl RedisSwapStore {
    /// Open a connection to the Redis URL (e.g.
    /// `redis://127.0.0.1:6379`). Errors if Redis is unreachable.
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

/// Lua script: SET row + ZADD on both indexes in a single redis-side
/// op. KEYS[1]=row, KEYS[2]=by-bob index, KEYS[3]=by-alice index;
/// ARGV[1]=json, ARGV[2]=swap_id, ARGV[3]=created_at_unix.
const UPSERT_SCRIPT: &str = r#"
redis.call('SET', KEYS[1], ARGV[1])
redis.call('ZADD', KEYS[2], ARGV[3], ARGV[2])
redis.call('ZADD', KEYS[3], ARGV[3], ARGV[2])
return 1
"#;

#[async_trait]
impl SwapStore for RedisSwapStore {
    async fn upsert(&self, tx: &Transaction) -> Result<()> {
        let json =
            serde_json::to_string(tx).map_err(|e| RefereeError::Store(format!("encode: {e}")))?;
        let row_key = swap_key(&tx.swap_id);
        let bob_key = by_bob_key(&tx.bob_pgp_fp);
        let alice_key = by_alice_key(&tx.alice_pgp_fp);
        let mut conn = self.conn.lock().await;
        let _: i64 = redis::Script::new(UPSERT_SCRIPT)
            .key(row_key)
            .key(bob_key)
            .key(alice_key)
            .arg(json)
            .arg(tx.swap_id.0.clone())
            .arg(tx.created_at_unix)
            .invoke_async(&mut *conn)
            .await
            .map_err(|e| RefereeError::Store(format!("redis upsert: {e}")))?;
        Ok(())
    }

    async fn get(&self, id: &SwapId) -> Result<Option<Transaction>> {
        let mut conn = self.conn.lock().await;
        let raw: Option<String> = conn
            .get(swap_key(id))
            .await
            .map_err(|e| RefereeError::Store(format!("redis get: {e}")))?;
        match raw {
            None => Ok(None),
            Some(s) => {
                Ok(Some(serde_json::from_str(&s).map_err(|e| {
                    RefereeError::Store(format!("decode: {e}"))
                })?))
            }
        }
    }

    async fn list_by_party(&self, fp: &PgpFingerprint) -> Result<Vec<TransactionSummary>> {
        let mut conn = self.conn.lock().await;
        // ZREVRANGEBYSCORE returns members sorted by score descending —
        // newest first.
        let bob_ids: Vec<String> = conn
            .zrevrangebyscore_limit(by_bob_key(fp), "+inf", "-inf", 0, 1000)
            .await
            .map_err(|e| RefereeError::Store(format!("redis zrev bob: {e}")))?;
        let alice_ids: Vec<String> = conn
            .zrevrangebyscore_limit(by_alice_key(fp), "+inf", "-inf", 0, 1000)
            .await
            .map_err(|e| RefereeError::Store(format!("redis zrev alice: {e}")))?;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(bob_ids.len() + alice_ids.len());
        for id in bob_ids.iter().chain(alice_ids.iter()) {
            if !seen.insert(id.clone()) {
                continue;
            }
            let raw: Option<String> = conn
                .get(format!("{SWAP_KEY_PREFIX}{}", id))
                .await
                .map_err(|e| RefereeError::Store(format!("redis fetch row: {e}")))?;
            if let Some(s) = raw {
                let tx: Transaction = serde_json::from_str(&s)
                    .map_err(|e| RefereeError::Store(format!("decode row: {e}")))?;
                let role = if tx.bob_pgp_fp == *fp && tx.alice_pgp_fp == *fp {
                    PartyRole::Both
                } else if tx.bob_pgp_fp == *fp {
                    PartyRole::Bob
                } else {
                    PartyRole::Alice
                };
                out.push(tx.summary(role));
            }
        }
        out.sort_by(|a, b| b.created_at_unix.cmp(&a.created_at_unix));
        out.truncate(1000);
        Ok(out)
    }

    async fn mark_terminal(&self, _id: &SwapId) -> Result<()> {
        // Terminal marking is a no-op on Redis; the row stays under
        // its key. A future GC sweep can prune by `updated_at_unix`.
        Ok(())
    }
}
