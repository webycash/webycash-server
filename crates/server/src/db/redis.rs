use async_trait::async_trait;
use redis::AsyncCommands;

use super::{BurnRecord, LedgerStore, ReplaceOp, ReplaceResult, TokenRecord};
use crate::protocol::mining::MiningState;

/// Number of parallel Redis connections for concurrent pipelined operations.
const POOL_SIZE: usize = 16;

pub struct RedisStore {
    conns: Vec<redis::aio::ConnectionManager>,
    conn_idx: std::sync::atomic::AtomicUsize,
    replace_sha: String,
    burn_sha: String,
}

impl RedisStore {
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;

        let conns = futures::future::try_join_all(
            (0..POOL_SIZE).map(|_| redis::aio::ConnectionManager::new(client.clone())),
        )
        .await?;

        // Pre-load Lua scripts (EVALSHA — no script transfer per call)
        let replace_sha = redis::Script::new(ATOMIC_REPLACE_LUA)
            .prepare_invoke()
            .load_async(&mut conns[0].clone())
            .await?;
        let burn_sha = redis::Script::new(BURN_LUA)
            .prepare_invoke()
            .load_async(&mut conns[0].clone())
            .await?;

        tracing::info!(
            pool = POOL_SIZE,
            replace_sha = %replace_sha,
            "Redis: {POOL_SIZE} connections, EVALSHA mode"
        );

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

    fn token_key(hash: &str) -> String {
        format!("token:{hash}")
    }

    const MINING_STATE_KEY: &'static str = "mining:state";
    const AUDIT_PREFIX: &'static str = "audit:";
}

/// Lua: atomic replace. Verifies, marks spent, inserts outputs, writes audit.
const ATOMIC_REPLACE_LUA: &str = r#"
local num_inputs = tonumber(ARGV[1])
local num_outputs = tonumber(ARGV[2])
local now = ARGV[3]
local audit_key = ARGV[4]
local audit_json = ARGV[5]
for i = 1, num_inputs do
    local key = KEYS[i]
    local json = redis.call('GET', key)
    if not json then
        return redis.error_reply('input token not found: ' .. key)
    end
    local record = cjson.decode(json)
    if record.spent then
        return redis.error_reply('input token already spent: ' .. key)
    end
end
for i = 1, num_inputs do
    local key = KEYS[i]
    local json = redis.call('GET', key)
    local record = cjson.decode(json)
    record.spent = true
    record.spent_at = now
    redis.call('SET', key, cjson.encode(record))
end
for i = 1, num_outputs do
    local key = KEYS[num_inputs + i]
    local existing = redis.call('EXISTS', key)
    if existing == 1 then
        return redis.error_reply('output token already exists: ' .. key)
    end
    local output_json = ARGV[5 + i]
    redis.call('SET', key, output_json)
end
redis.call('SET', audit_key, audit_json)
return 'ok'
"#;

/// Lua: atomic burn. Verifies, marks spent, writes audit.
const BURN_LUA: &str = r#"
local token_key = KEYS[1]
local audit_key = KEYS[2]
local now = ARGV[1]
local audit_json = ARGV[2]
local json = redis.call('GET', token_key)
if not json then
    return redis.error_reply('token not found')
end
local record = cjson.decode(json)
if record.spent then
    return redis.error_reply('token already spent')
end
record.spent = true
record.spent_at = now
redis.call('SET', token_key, cjson.encode(record))
redis.call('SET', audit_key, audit_json)
return 'ok'
"#;

#[async_trait]
impl LedgerStore for RedisStore {
    // ── Batch token insert (pipelined SET NX) ────────────────────────

    async fn insert_tokens(&self, records: &[TokenRecord]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        // Pipeline all SET NX commands in one round-trip
        let mut conn = self.conn();
        let mut pipe = redis::pipe();

        let serialized: Vec<(String, String)> = records
            .iter()
            .map(|r| Ok((Self::token_key(&r.public_hash), serde_json::to_string(r)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;

        serialized.iter().for_each(|(key, json)| {
            pipe.cmd("SET").arg(key).arg(json).arg("NX");
        });

        let results: Vec<bool> = pipe.query_async(&mut conn).await?;

        // Check for duplicates
        results
            .iter()
            .zip(records.iter())
            .find(|(ok, _)| !**ok)
            .map(|(_, r)| anyhow::bail!("token already exists: {}", r.public_hash))
            .unwrap_or(Ok(()))
    }

    // ── Batch token lookup (pipelined GET) ───────────────────────────

    async fn get_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<Option<TokenRecord>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = self.conn();
        let mut pipe = redis::pipe();

        let keys: Vec<String> = hashes.iter().map(|h| Self::token_key(h)).collect();
        keys.iter().for_each(|k| {
            pipe.cmd("GET").arg(k);
        });

        let jsons: Vec<Option<String>> = pipe.query_async(&mut conn).await?;

        jsons
            .into_iter()
            .map(|j| match j {
                Some(s) => Ok(Some(serde_json::from_str(&s)?)),
                None => Ok(None),
            })
            .collect()
    }

    // ── Batch check tokens (pipelined GET + extract spent) ───────────

    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = self.conn();
        let mut pipe = redis::pipe();

        let keys: Vec<String> = hashes.iter().map(|h| Self::token_key(h)).collect();
        keys.iter().for_each(|k| {
            pipe.cmd("GET").arg(k);
        });

        let jsons: Vec<Option<String>> = pipe.query_async(&mut conn).await?;

        Ok(hashes
            .iter()
            .zip(jsons)
            .map(|(hash, json_opt)| {
                let status = json_opt.and_then(|j| {
                    serde_json::from_str::<TokenRecord>(&j)
                        .map(|r| r.spent)
                        .ok()
                });
                (hash.clone(), status)
            })
            .collect())
    }

    // ── Batch replace (parallel EVALSHA across connection pool) ──────

    async fn batch_replace(&self, ops: &[ReplaceOp]) -> Vec<ReplaceResult> {
        if ops.is_empty() {
            return Vec::new();
        }

        // Execute all replaces concurrently across the connection pool.
        // Each EVALSHA goes to a different connection — maximum parallelism.
        futures::future::join_all(ops.iter().map(|op| {
            let conn = self.conn();
            let sha = self.replace_sha.clone();
            async move {
                match Self::exec_replace(conn, &sha, op).await {
                    Ok(()) => ReplaceResult::Ok,
                    Err(e) => ReplaceResult::Failed(e.to_string()),
                }
            }
        }))
        .await
    }

    // ── Batch burn (parallel EVALSHA across connection pool) ─────────

    async fn batch_burn(&self, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }

        // Parallel burns across connection pool
        let results: Vec<anyhow::Result<()>> =
            futures::future::join_all(ops.iter().map(|(hash, record)| {
                let conn = self.conn();
                let sha = self.burn_sha.clone();
                let token_key = Self::token_key(hash);
                let audit_key = format!("{}burn:{}", Self::AUDIT_PREFIX, record.id);
                let now = chrono::Utc::now().to_rfc3339();
                let audit_json = serde_json::to_string(record).unwrap_or_default();
                async move {
                    let _: String = redis::cmd("EVALSHA")
                        .arg(&sha)
                        .arg(2)
                        .arg(&token_key)
                        .arg(&audit_key)
                        .arg(&now)
                        .arg(&audit_json)
                        .query_async(&mut conn.clone())
                        .await
                        .map_err(|e| anyhow::anyhow!("burn failed: {e}"))?;
                    Ok(())
                }
            }))
            .await;

        // Return first error if any
        results.into_iter().collect()
    }

    // ── Mining state ─────────────────────────────────────────────────

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let mut conn = self.conn();
        let json: Option<String> = conn.get(Self::MINING_STATE_KEY).await?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let json = serde_json::to_string(state)?;
        conn.set::<_, _, ()>(Self::MINING_STATE_KEY, &json).await?;
        Ok(())
    }

    // get_stats: uses default trait method
}

impl RedisStore {
    /// Execute a single atomic replace via EVALSHA.
    async fn exec_replace(
        mut conn: redis::aio::ConnectionManager,
        sha: &str,
        op: &ReplaceOp,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();

        let input_keys: Vec<String> = op.inputs.iter().map(|h| Self::token_key(h)).collect();
        let output_keys: Vec<String> = op
            .outputs
            .iter()
            .map(|o| Self::token_key(&o.public_hash))
            .collect();
        let num_keys = input_keys.len() + output_keys.len();

        let audit_key = format!("{}replace:{}", Self::AUDIT_PREFIX, op.record.id);
        let audit_json = serde_json::to_string(&op.record)?;
        let output_jsons: Vec<String> = op
            .outputs
            .iter()
            .map(serde_json::to_string)
            .collect::<serde_json::Result<Vec<_>>>()?;

        let mut cmd = redis::cmd("EVALSHA");
        cmd.arg(sha).arg(num_keys);

        input_keys.iter().chain(output_keys.iter()).for_each(|k| {
            cmd.arg(k);
        });

        cmd.arg(op.inputs.len().to_string())
            .arg(op.outputs.len().to_string())
            .arg(&now)
            .arg(&audit_key)
            .arg(&audit_json);

        output_jsons.iter().for_each(|j| {
            cmd.arg(j);
        });

        let result: String = cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| anyhow::anyhow!("atomic replace failed: {e}"))?;

        if result != "ok" {
            anyhow::bail!("atomic replace failed: {result}");
        }
        Ok(())
    }
}
