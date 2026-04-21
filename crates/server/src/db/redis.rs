use async_trait::async_trait;
use redis::AsyncCommands;

use super::{BurnRecord, LedgerStore, ReplaceOp, ReplaceResult, TokenRecord};
use crate::protocol::mining::MiningState;

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
            "Redis: {POOL_SIZE} connections, EVALSHA pipelined"
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

/// Lua: atomic replace. Returns "ok" or "ERR:reason" (string, not error_reply).
/// Using string returns instead of redis.error_reply() allows pipelining
/// multiple EVALSHA calls without aborting the pipeline on first failure.
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
        return 'ERR:input token not found: ' .. key
    end
    local record = cjson.decode(json)
    if record.spent then
        return 'ERR:input token already spent: ' .. key
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
        return 'ERR:output token already exists: ' .. key
    end
    local output_json = ARGV[5 + i]
    redis.call('SET', key, output_json)
end
redis.call('SET', audit_key, audit_json)
return 'ok'
"#;

/// Lua: atomic burn. Returns "ok" or "ERR:reason" (pipeline-safe).
const BURN_LUA: &str = r#"
local token_key = KEYS[1]
local audit_key = KEYS[2]
local now = ARGV[1]
local audit_json = ARGV[2]
local json = redis.call('GET', token_key)
if not json then
    return 'ERR:token not found'
end
local record = cjson.decode(json)
if record.spent then
    return 'ERR:token already spent'
end
record.spent = true
record.spent_at = now
redis.call('SET', token_key, cjson.encode(record))
redis.call('SET', audit_key, audit_json)
return 'ok'
"#;

#[async_trait]
impl LedgerStore for RedisStore {
    async fn insert_tokens(&self, records: &[TokenRecord]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
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

        results
            .iter()
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
            pipe.cmd("GET").arg(Self::token_key(h));
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

    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();

        hashes.iter().for_each(|h| {
            pipe.cmd("GET").arg(Self::token_key(h));
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

    // ── PIPELINED batch replace: ALL ops in ONE Redis round-trip ─────

    async fn batch_replace(&self, ops: &[ReplaceOp]) -> Vec<ReplaceResult> {
        if ops.is_empty() {
            return Vec::new();
        }

        // Build a SINGLE pipeline containing ALL EVALSHA calls.
        // One round-trip for the entire batch — Redis executes them sequentially
        // but network overhead is ONE RTT total instead of N RTTs.
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        let now = chrono::Utc::now().to_rfc3339();

        ops.iter().for_each(|op| {
            let input_keys: Vec<String> = op.inputs.iter().map(|h| Self::token_key(h)).collect();
            let output_keys: Vec<String> = op
                .outputs
                .iter()
                .map(|o| Self::token_key(&o.public_hash))
                .collect();
            let num_keys = input_keys.len() + output_keys.len();
            let audit_key = format!("{}replace:{}", Self::AUDIT_PREFIX, op.record.id);
            let audit_json = serde_json::to_string(&op.record).unwrap_or_default();
            let output_jsons: Vec<String> = op
                .outputs
                .iter()
                .map(|o| serde_json::to_string(o).unwrap_or_default())
                .collect();

            let cmd = pipe.cmd("EVALSHA").arg(&self.replace_sha).arg(num_keys);

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
        });

        // ONE round-trip for ALL operations
        let results: Result<Vec<redis::Value>, _> = pipe.query_async(&mut conn).await;

        match results {
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

    // ── PIPELINED batch burn ─────────────────────────────────────────

    async fn batch_burn(&self, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }

        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        let now = chrono::Utc::now().to_rfc3339();

        ops.iter().for_each(|(hash, record)| {
            let token_key = Self::token_key(hash);
            let audit_key = format!("{}burn:{}", Self::AUDIT_PREFIX, record.id);
            let audit_json = serde_json::to_string(record).unwrap_or_default();

            pipe.cmd("EVALSHA")
                .arg(&self.burn_sha)
                .arg(2)
                .arg(&token_key)
                .arg(&audit_key)
                .arg(&now)
                .arg(&audit_json);
        });

        let results: Vec<redis::Value> = pipe
            .query_async(&mut conn)
            .await
            .map_err(|e| anyhow::anyhow!("batch burn pipeline failed: {e}"))?;

        // Check for first error (ERR: prefix from Lua string return)
        results
            .iter()
            .find_map(|v| {
                let s = match v {
                    redis::Value::SimpleString(s) => s.as_str(),
                    redis::Value::BulkString(b) => std::str::from_utf8(b).unwrap_or(""),
                    _ => "",
                };
                s.strip_prefix("ERR:")
                    .map(|err| anyhow::anyhow!("burn failed: {err}"))
            })
            .map_or(Ok(()), Err)
    }

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
}
