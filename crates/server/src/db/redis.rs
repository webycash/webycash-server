//! Redis backend — native HASH storage, Lua for atomicity, pipelined batches.
//!
//! Tokens: Redis HASHes `token:{hash}` with fields: amount_wats, spent, created_at, spent_at, origin
//! Replace: Lua EVALSHA operates on HASH fields (no JSON encode/decode in Redis)
//! Reads: native HGET/HGETALL pipelined (benefits from io-threads)

use async_trait::async_trait;
use redis::AsyncCommands;

use super::{BurnRecord, LedgerStore, ReplaceOp, ReplaceResult, TokenOrigin, TokenRecord};
use crate::protocol::mining::MiningState;

const POOL_SIZE: usize = 16;

pub struct RedisStore {
    conns: Vec<redis::aio::ConnectionManager>,
    conn_idx: std::sync::atomic::AtomicUsize,
    replace_sha: String,
    burn_sha: String,
}

/// Lua: atomic replace on HASH-stored tokens. Returns "ok" or "ERR:reason".
/// Operates directly on HASH fields — no JSON encoding in Redis.
const REPLACE_LUA: &str = r#"
local ni = tonumber(ARGV[1])
local no = tonumber(ARGV[2])
local now = ARGV[3]
local audit_key = ARGV[4]
local audit_json = ARGV[5]
-- Validate inputs exist and unspent
for i = 1, ni do
    local k = KEYS[i]
    if redis.call('EXISTS', k) == 0 then return 'ERR:input token not found: ' .. k end
    if redis.call('HGET', k, 'spent') == '1' then return 'ERR:input token already spent: ' .. k end
end
-- Mark inputs spent
for i = 1, ni do
    redis.call('HSET', KEYS[i], 'spent', '1', 'spent_at', now)
end
-- Insert outputs (ARGV[6..] = amount_wats, created_at, origin for each output)
for i = 1, no do
    local k = KEYS[ni + i]
    if redis.call('EXISTS', k) == 1 then return 'ERR:output token already exists: ' .. k end
    local base = 5 + (i - 1) * 3
    redis.call('HSET', k, 'amount_wats', ARGV[base + 1], 'spent', '0',
               'created_at', ARGV[base + 2], 'origin', ARGV[base + 3])
end
redis.call('SET', audit_key, audit_json)
return 'ok'
"#;

/// Lua: atomic burn on HASH-stored tokens.
const BURN_LUA: &str = r#"
local tk = KEYS[1]
local ak = KEYS[2]
local now = ARGV[1]
local aj = ARGV[2]
if redis.call('EXISTS', tk) == 0 then return 'ERR:token not found' end
if redis.call('HGET', tk, 'spent') == '1' then return 'ERR:token already spent' end
redis.call('HSET', tk, 'spent', '1', 'spent_at', now)
redis.call('SET', ak, aj)
return 'ok'
"#;

impl RedisStore {
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let conns = futures::future::try_join_all(
            (0..POOL_SIZE).map(|_| redis::aio::ConnectionManager::new(client.clone())),
        )
        .await?;

        let replace_sha = redis::Script::new(REPLACE_LUA)
            .prepare_invoke()
            .load_async(&mut conns[0].clone())
            .await?;
        let burn_sha = redis::Script::new(BURN_LUA)
            .prepare_invoke()
            .load_async(&mut conns[0].clone())
            .await?;

        tracing::info!(pool = POOL_SIZE, "Redis: HASH storage, EVALSHA atomicity");
        Ok(Self {
            conns,
            conn_idx: std::sync::atomic::AtomicUsize::new(0),
            replace_sha,
            burn_sha,
        })
    }

    fn conn(&self) -> redis::aio::ConnectionManager {
        let idx = self
            .conn_idx
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.conns.len();
        self.conns[idx].clone()
    }

    fn key(hash: &str) -> String {
        format!("token:{hash}")
    }

    fn hash_to_token(
        hash: &str,
        fields: &std::collections::HashMap<String, String>,
    ) -> Option<TokenRecord> {
        Some(TokenRecord {
            public_hash: hash.to_string(),
            amount_wats: fields.get("amount_wats")?.parse().ok()?,
            spent: fields.get("spent").map(|s| s == "1").unwrap_or(false),
            created_at: fields
                .get("created_at")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now),
            spent_at: fields
                .get("spent_at")
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            origin: match fields.get("origin").map(|s| s.as_str()) {
                Some("replaced") => TokenOrigin::Replaced,
                _ => TokenOrigin::Mined,
            },
        })
    }
}

#[async_trait]
impl LedgerStore for RedisStore {
    async fn insert_tokens(&self, records: &[TokenRecord]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();

        // HSETNX on amount_wats to detect duplicates, then HSET remaining fields
        records.iter().for_each(|r| {
            pipe.cmd("HSETNX")
                .arg(Self::key(&r.public_hash))
                .arg("amount_wats")
                .arg(r.amount_wats.to_string());
        });
        let nx: Vec<bool> = pipe.query_async(&mut conn).await?;

        // Set remaining fields for new tokens
        let mut set_pipe = redis::pipe();
        nx.iter().zip(records.iter()).for_each(|(created, r)| {
            if *created {
                let k = Self::key(&r.public_hash);
                set_pipe
                    .cmd("HSET")
                    .arg(&k)
                    .arg("spent")
                    .arg(if r.spent { "1" } else { "0" })
                    .arg("created_at")
                    .arg(r.created_at.to_rfc3339())
                    .arg("origin")
                    .arg(match r.origin {
                        TokenOrigin::Mined => "mined",
                        TokenOrigin::Replaced => "replaced",
                    })
                    .ignore();
            }
        });
        if set_pipe.cmd_iter().count() > 0 {
            let _: Vec<redis::Value> = set_pipe.query_async(&mut conn).await?;
        }

        nx.iter()
            .zip(records.iter())
            .find(|(ok, _)| !**ok)
            .map(|(_, r)| anyhow::bail!("token already exists: {}", r.public_hash))
            .unwrap_or(Ok(()))
    }

    async fn get_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<Option<TokenRecord>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        hashes.iter().for_each(|h| {
            pipe.cmd("HGETALL").arg(Self::key(h));
        });
        let results: Vec<std::collections::HashMap<String, String>> =
            pipe.query_async(&mut conn).await?;
        Ok(hashes
            .iter()
            .zip(results)
            .map(|(h, f)| {
                if f.is_empty() {
                    None
                } else {
                    Self::hash_to_token(h, &f)
                }
            })
            .collect())
    }

    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        hashes.iter().for_each(|h| {
            pipe.cmd("HGET").arg(Self::key(h)).arg("spent");
        });
        let results: Vec<Option<String>> = pipe.query_async(&mut conn).await?;
        Ok(hashes
            .iter()
            .zip(results)
            .map(|(h, s)| (h.clone(), s.map(|v| v == "1")))
            .collect())
    }

    /// Pipelined batch replace: ALL EVALSHA in ONE round-trip.
    async fn batch_replace(&self, ops: &[ReplaceOp]) -> Vec<ReplaceResult> {
        if ops.is_empty() {
            return Vec::new();
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        let now = chrono::Utc::now().to_rfc3339();

        ops.iter().for_each(|op| {
            let ik: Vec<String> = op.inputs.iter().map(|h| Self::key(h)).collect();
            let ok: Vec<String> = op
                .outputs
                .iter()
                .map(|o| Self::key(&o.public_hash))
                .collect();
            let nk = ik.len() + ok.len();
            let audit_key = format!("audit:replace:{}", op.record.id);
            let audit_json = serde_json::to_string(&op.record).unwrap_or_default();

            let cmd = pipe.cmd("EVALSHA").arg(&self.replace_sha).arg(nk);
            ik.iter().chain(ok.iter()).for_each(|k| {
                cmd.arg(k);
            });
            cmd.arg(op.inputs.len().to_string())
                .arg(op.outputs.len().to_string())
                .arg(&now)
                .arg(&audit_key)
                .arg(&audit_json);
            // Output fields: amount_wats, created_at, origin per output
            op.outputs.iter().for_each(|o| {
                cmd.arg(o.amount_wats.to_string())
                    .arg(o.created_at.to_rfc3339())
                    .arg(match o.origin {
                        TokenOrigin::Mined => "mined",
                        TokenOrigin::Replaced => "replaced",
                    });
            });
        });

        match pipe.query_async::<Vec<redis::Value>>(&mut conn).await {
            Ok(values) => values
                .into_iter()
                .map(|v| {
                    let s = match v {
                        redis::Value::SimpleString(s) => s,
                        redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
                        other => return ReplaceResult::Failed(format!("unexpected: {other:?}")),
                    };
                    if s == "ok" {
                        ReplaceResult::Ok
                    } else if let Some(err) = s.strip_prefix("ERR:") {
                        ReplaceResult::Failed(err.to_string())
                    } else {
                        ReplaceResult::Failed(s)
                    }
                })
                .collect(),
            Err(e) => ops
                .iter()
                .map(|_| ReplaceResult::Failed(e.to_string()))
                .collect(),
        }
    }

    async fn batch_burn(&self, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        let now = chrono::Utc::now().to_rfc3339();

        ops.iter().for_each(|(hash, record)| {
            pipe.cmd("EVALSHA")
                .arg(&self.burn_sha)
                .arg(2)
                .arg(Self::key(hash))
                .arg(format!("audit:burn:{}", record.id))
                .arg(&now)
                .arg(serde_json::to_string(record).unwrap_or_default());
        });

        let results: Vec<redis::Value> = pipe
            .query_async(&mut conn)
            .await
            .map_err(|e| anyhow::anyhow!("batch burn failed: {e}"))?;

        results
            .iter()
            .find_map(|v| {
                let s = match v {
                    redis::Value::SimpleString(s) => s.as_str(),
                    redis::Value::BulkString(b) => std::str::from_utf8(b).unwrap_or(""),
                    _ => "",
                };
                s.strip_prefix("ERR:")
                    .map(|e| anyhow::anyhow!("burn failed: {e}"))
            })
            .map_or(Ok(()), Err)
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let mut conn = self.conn();
        let json: Option<String> = conn.get("mining:state").await?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let json = serde_json::to_string(state)?;
        conn.set::<_, _, ()>("mining:state", &json).await?;
        Ok(())
    }
}
