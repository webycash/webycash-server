use async_trait::async_trait;
use redis::AsyncCommands;

use super::{BurnRecord, LedgerStore, ReplacementRecord, TokenRecord};
use crate::protocol::mining::MiningState;

pub struct RedisStore {
    pool: redis::aio::ConnectionManager,
}

impl RedisStore {
    pub async fn new(url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(url)?;
        let pool = redis::aio::ConnectionManager::new(client).await?;
        Ok(Self { pool })
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
        let mut conn = self.pool.clone();
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
        let mut conn = self.pool.clone();
        let key = Self::token_key(public_hash);
        let json: Option<String> = conn.get(&key).await?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    async fn mark_spent(&self, public_hash: &str) -> anyhow::Result<bool> {
        let mut conn = self.pool.clone();
        let key = Self::token_key(public_hash);
        let now = chrono::Utc::now().to_rfc3339();

        // Atomic via Lua script — no TOCTOU race
        let result: i32 = redis::cmd("EVAL")
            .arg(MARK_SPENT_LUA)
            .arg(1) // num keys
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
        let mut conn = self.pool.clone();
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

        // Build EVAL command with chained iterators
        let mut cmd = redis::cmd("EVAL");
        cmd.arg(ATOMIC_REPLACE_LUA).arg(num_keys);

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
        let mut conn = self.pool.clone();
        let json: Option<String> = conn.get(Self::MINING_STATE_KEY).await?;
        match json {
            Some(j) => Ok(Some(serde_json::from_str(&j)?)),
            None => Ok(None),
        }
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let mut conn = self.pool.clone();
        let json = serde_json::to_string(state)?;
        conn.set::<_, _, ()>(Self::MINING_STATE_KEY, &json).await?;
        Ok(())
    }

    async fn burn_token(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        let mut conn = self.pool.clone();
        let token_key = Self::token_key(public_hash);
        let audit_key = format!("{}burn:{}", Self::AUDIT_PREFIX, record.id);
        let now = chrono::Utc::now().to_rfc3339();
        let audit_json = serde_json::to_string(record)?;

        // Atomic via Lua script — no TOCTOU race
        let _: String = redis::cmd("EVAL")
            .arg(BURN_LUA)
            .arg(2) // num keys
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
        let mut conn = self.pool.clone();

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
