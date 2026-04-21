use async_trait::async_trait;
use redis::AsyncCommands;

use super::{BurnRecord, LedgerStore, ReplacementRecord, TokenRecord};
use crate::protocol::mining::MiningState;

/// Number of parallel Redis connections for concurrent operations.
const REDIS_POOL_SIZE: usize = 16;

pub struct RedisStore {
    /// Multiple connections for parallel in-flight operations.
    /// Round-robin across pool for maximum throughput.
    conns: Vec<redis::aio::ConnectionManager>,
    conn_idx: std::sync::atomic::AtomicUsize,
    /// Pre-loaded script SHA for EVALSHA (eliminates script transfer per call)
    replace_sha: String,
    burn_sha: String,
    mark_spent_sha: String,
}

impl RedisStore {
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;

        // Create pool of connections for parallel operations
        let conns = futures::future::try_join_all(
            (0..REDIS_POOL_SIZE).map(|_| redis::aio::ConnectionManager::new(client.clone())),
        )
        .await?;

        // Pre-load Lua scripts — EVALSHA uses SHA1 hash (no script transfer per call)
        let replace_sha = redis::Script::new(ATOMIC_REPLACE_LUA)
            .prepare_invoke()
            .load_async(&mut conns[0].clone())
            .await?;
        let burn_sha = redis::Script::new(BURN_LUA)
            .prepare_invoke()
            .load_async(&mut conns[0].clone())
            .await?;
        let mark_spent_sha = redis::Script::new(MARK_SPENT_LUA)
            .prepare_invoke()
            .load_async(&mut conns[0].clone())
            .await?;

        tracing::info!(
            pool_size = REDIS_POOL_SIZE,
            replace_sha = %replace_sha,
            "Redis initialized ({REDIS_POOL_SIZE} connections, EVALSHA mode)"
        );

        Ok(Self {
            conns,
            conn_idx: std::sync::atomic::AtomicUsize::new(0),
            replace_sha,
            burn_sha,
            mark_spent_sha,
        })
    }

    /// Get next connection from the round-robin pool.
    fn conn(&self) -> redis::aio::ConnectionManager {
        let idx = self
            .conn_idx
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.conns.len();
        self.conns[idx].clone()
    }

    fn token_key(hash: &str) -> String {
        format!("token:{}", hash)
    }

    const MINING_STATE_KEY: &'static str = "mining:state";
    const AUDIT_PREFIX: &'static str = "audit:";
}

/// Lua script for atomic mark_spent.
/// Returns 1 if marked spent, 0 if already spent or not found.
/// Atomicity guaranteed: single Lua script execution is atomic in Redis.
const MARK_SPENT_LUA: &str = r#"
local key = KEYS[1]
local json = redis.call('GET', key)
if not json then return 0 end
local record = cjson.decode(json)
if record.spent then return 0 end
record.spent = true
record.spent_at = ARGV[1]
redis.call('SET', key, cjson.encode(record))
return 1
"#;

/// Lua script for atomic replace.
/// Verifies all inputs exist and are unspent, then atomically
/// marks inputs spent, inserts outputs, and writes audit record.
/// Returns "ok" on success, error string on failure.
const ATOMIC_REPLACE_LUA: &str = r#"
local num_inputs = tonumber(ARGV[1])
local num_outputs = tonumber(ARGV[2])
local now = ARGV[3]
local audit_key = ARGV[4]
local audit_json = ARGV[5]

-- Verify all inputs exist and are unspent
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

-- Mark all inputs spent (atomic within this script)
for i = 1, num_inputs do
    local key = KEYS[i]
    local json = redis.call('GET', key)
    local record = cjson.decode(json)
    record.spent = true
    record.spent_at = now
    redis.call('SET', key, cjson.encode(record))
end

-- Insert all outputs (fail if any already exists)
for i = 1, num_outputs do
    local key = KEYS[num_inputs + i]
    local existing = redis.call('EXISTS', key)
    if existing == 1 then
        return redis.error_reply('output token already exists: ' .. key)
    end
    local output_json = ARGV[5 + i]
    redis.call('SET', key, output_json)
end

-- Write audit record
redis.call('SET', audit_key, audit_json)
return 'ok'
"#;

/// Lua script for atomic burn.
/// Verifies token exists and is unspent, then marks spent and writes audit.
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
    async fn insert_token(&self, record: &TokenRecord) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let key = Self::token_key(&record.public_hash);
        let json = serde_json::to_string(record)?;

        // SET NX — fail if key already exists (prevents duplicate tokens)
        let set: bool = redis::cmd("SET")
            .arg(&key)
            .arg(&json)
            .arg("NX")
            .query_async(&mut conn)
            .await?;

        if !set {
            anyhow::bail!("token already exists: {}", record.public_hash);
        }
        Ok(())
    }

    async fn get_token(&self, public_hash: &str) -> anyhow::Result<Option<TokenRecord>> {
        let mut conn = self.conn();
        let key = Self::token_key(public_hash);
        let json: Option<String> = conn.get(&key).await?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    async fn mark_spent(&self, public_hash: &str) -> anyhow::Result<bool> {
        let mut conn = self.conn();
        let key = Self::token_key(public_hash);
        let now = chrono::Utc::now().to_rfc3339();

        // EVALSHA: pre-cached Lua script — no TOCTOU race
        let result: i32 = redis::cmd("EVALSHA")
            .arg(&self.mark_spent_sha)
            .arg(1)
            .arg(&key)
            .arg(&now)
            .query_async(&mut conn)
            .await?;

        Ok(result == 1)
    }

    async fn atomic_replace(
        &self,
        inputs: &[String],
        outputs: &[TokenRecord],
        record: &ReplacementRecord,
    ) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let now = chrono::Utc::now().to_rfc3339();

        // Build keys: [input_keys..., output_keys...]
        let input_keys: Vec<String> = inputs.iter().map(|h| Self::token_key(h)).collect();
        let output_keys: Vec<String> = outputs
            .iter()
            .map(|o| Self::token_key(&o.public_hash))
            .collect();

        let num_keys = input_keys.len() + output_keys.len();
        let audit_key = format!("{}replace:{}", Self::AUDIT_PREFIX, record.id);
        let audit_json = serde_json::to_string(record)?;

        // Pre-serialize output records
        let output_jsons: Vec<String> = outputs
            .iter()
            .map(serde_json::to_string)
            .collect::<serde_json::Result<Vec<_>>>()?;

        // EVALSHA: pre-cached script SHA, no script transfer per call
        let mut cmd = redis::cmd("EVALSHA");
        cmd.arg(&self.replace_sha).arg(num_keys);

        // Keys: inputs then outputs (single chain)
        input_keys.iter().chain(output_keys.iter()).for_each(|k| {
            cmd.arg(k);
        });

        // ARGV: [num_inputs, num_outputs, now, audit_key, audit_json, output_jsons...]
        cmd.arg(inputs.len().to_string())
            .arg(outputs.len().to_string())
            .arg(&now)
            .arg(&audit_key)
            .arg(&audit_json);

        output_jsons.iter().for_each(|j| {
            cmd.arg(j);
        });

        let result: String = cmd
            .query_async(&mut conn)
            .await
            .map_err(|e| anyhow::anyhow!("atomic replace failed: {}", e))?;

        if result != "ok" {
            anyhow::bail!("atomic replace failed: {}", result);
        }
        Ok(())
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let mut conn = self.conn();
        let json: Option<String> = conn.get(Self::MINING_STATE_KEY).await?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let json = serde_json::to_string(state)?;
        conn.set::<_, _, ()>(Self::MINING_STATE_KEY, &json).await?;
        Ok(())
    }

    async fn burn_token(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let token_key = Self::token_key(public_hash);
        let audit_key = format!("{}burn:{}", Self::AUDIT_PREFIX, record.id);
        let now = chrono::Utc::now().to_rfc3339();
        let audit_json = serde_json::to_string(record)?;

        // EVALSHA: pre-cached Lua script — no TOCTOU race
        let _: String = redis::cmd("EVALSHA")
            .arg(&self.burn_sha)
            .arg(2)
            .arg(&token_key)
            .arg(&audit_key)
            .arg(&now)
            .arg(&audit_json)
            .query_async(&mut conn)
            .await
            .map_err(|e| anyhow::anyhow!("burn failed: {}", e))?;

        Ok(())
    }

    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        let mut conn = self.conn();

        // Pipeline: N sequential round-trips → 1 pipelined round-trip
        let keys: Vec<String> = hashes.iter().map(|h| Self::token_key(h)).collect();
        let mut pipe = redis::pipe();
        keys.iter().for_each(|k| {
            pipe.cmd("GET").arg(k);
        });
        let values: Vec<Option<String>> = pipe.query_async(&mut conn).await?;

        Ok(hashes
            .iter()
            .zip(values)
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

    // get_stats: uses default trait method (derived from mining state)
}
