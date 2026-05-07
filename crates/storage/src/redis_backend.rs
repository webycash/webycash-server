//! Generic Redis backend for `LedgerStore<A>`. Uses HASH storage with one
//! field per record attribute (no JSON encoding inside Redis), preserving
//! the legacy webycash testnet schema. Atomic replace + burn via EVALSHA.
//!
//! Generic over the asset type and the key strategy:
//!   - `RedisStore::<Webcash, WebcashLegacyKeys>::new(url)` → legacy keys
//!   - `RedisStore::<Rgb, NamespacedKeys>::new(url)` → asset-namespaced keys
//!
//! Records implement `HashRecord` to define how they round-trip through
//! the Redis HASH fields. Webcash's impl uses the historical fields:
//! `amount_wats, spent, created_at, spent_at, origin`.

use std::collections::HashMap;
use std::marker::PhantomData;

use crate::asset_core::Asset;
use async_trait::async_trait;
use redis::AsyncCommands;

use crate::storage::{
    BurnRecord, HashRecord, KeyStrategy, LedgerStore, MiningState, Namespace, ReplaceOp,
    ReplaceResult,
};

const POOL_SIZE: usize = 16;

const REPLACE_LUA: &str = r#"
local ni = tonumber(ARGV[1])
local no = tonumber(ARGV[2])
local fields_per_output = tonumber(ARGV[3])
local now = ARGV[4]
local audit_key = ARGV[5]
local audit_json = ARGV[6]
for i = 1, ni do
    local k = KEYS[i]
    if redis.call('EXISTS', k) == 0 then return 'ERR:input token not found: ' .. k end
    if redis.call('HGET', k, 'spent') == '1' then return 'ERR:input token already spent: ' .. k end
end
for i = 1, ni do
    redis.call('HSET', KEYS[i], 'spent', '1', 'spent_at', now)
end
for i = 1, no do
    local k = KEYS[ni + i]
    if redis.call('EXISTS', k) == 1 then return 'ERR:output token already exists: ' .. k end
    -- Output fields: name, value pairs starting at ARGV[6 + (i-1)*fields_per_output*2 + 1]
    local base = 6 + (i - 1) * fields_per_output * 2
    local args = {}
    for j = 1, fields_per_output * 2 do
        args[j] = ARGV[base + j]
    end
    redis.call('HSET', k, unpack(args))
end
redis.call('SET', audit_key, audit_json)
return 'ok'
"#;

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

/// Redis-backed `LedgerStore` implementation.
///
/// Uses a small connection pool (`POOL_SIZE`) and pre-loaded Lua scripts for
/// atomic replace and burn operations. Generic over the asset (`A`) and
/// key strategy (`K`) so the same code path serves Webcash legacy keys and
/// the namespaced `(asset, contract, issuer)` keys for RGB/Voucher.
pub struct RedisStore<A: Asset, K: KeyStrategy> {
    conns: Vec<redis::aio::ConnectionManager>,
    conn_idx: std::sync::atomic::AtomicUsize,
    replace_sha: String,
    burn_sha: String,
    keys: K,
    _ph: PhantomData<A>,
}

impl<A: Asset, K: KeyStrategy> RedisStore<A, K> {
    /// Open a new pool against `url`, load the Lua scripts, and return a
    /// store ready to serve requests.
    pub async fn new(url: &str, keys: K) -> anyhow::Result<Self> {
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

        tracing::info!(
            asset = A::NAME,
            pool = POOL_SIZE,
            "Redis: HASH storage, EVALSHA atomicity"
        );
        Ok(Self {
            conns,
            conn_idx: std::sync::atomic::AtomicUsize::new(0),
            replace_sha,
            burn_sha,
            keys,
            _ph: PhantomData,
        })
    }

    fn conn(&self) -> redis::aio::ConnectionManager {
        let idx = self
            .conn_idx
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            % self.conns.len();
        self.conns[idx].clone()
    }
}

#[async_trait]
impl<A: Asset, K: KeyStrategy> LedgerStore<A> for RedisStore<A, K>
where
    A::Record: HashRecord + serde::Serialize + serde::de::DeserializeOwned,
{
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();

        // Each record reports its own namespace (Webcash unscoped; RGB / Voucher
        // scoped on (contract_id, issuer_fp)). Storage keys are derived per record.
        records.iter().for_each(|r| {
            pipe.cmd("HSETNX")
                .arg(
                    self.keys
                        .token_key(A::NAME, &r.namespace(), r.public_hash()),
                )
                .arg("amount_wats")
                .arg(r.amount_wats().to_string());
        });
        let nx: Vec<bool> = pipe.query_async(&mut conn).await?;

        let mut set_pipe = redis::pipe();
        nx.iter().zip(records.iter()).for_each(|(created, r)| {
            if *created {
                let mut fields = HashMap::new();
                r.to_fields(&mut fields);
                fields.remove("amount_wats");
                let k = self
                    .keys
                    .token_key(A::NAME, &r.namespace(), r.public_hash());
                let cmd = set_pipe.cmd("HSET");
                cmd.arg(&k);
                fields.iter().for_each(|(name, value)| {
                    cmd.arg(name).arg(value);
                });
                cmd.ignore();
            }
        });
        if set_pipe.cmd_iter().count() > 0 {
            let _: Vec<redis::Value> = set_pipe.query_async(&mut conn).await?;
        }

        nx.iter()
            .zip(records.iter())
            .find(|(ok, _)| !**ok)
            .map(|(_, r)| anyhow::bail!("token already exists: {}", r.public_hash()))
            .unwrap_or(Ok(()))
    }

    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        hashes.iter().for_each(|h| {
            pipe.cmd("HGETALL").arg(self.keys.token_key(A::NAME, ns, h));
        });
        let results: Vec<HashMap<String, String>> = pipe.query_async(&mut conn).await?;
        Ok(hashes
            .iter()
            .zip(results)
            .map(|(h, fields)| {
                if fields.is_empty() {
                    None
                } else {
                    A::Record::from_fields(h, &fields)
                }
            })
            .collect())
    }

    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        hashes.iter().for_each(|h| {
            pipe.cmd("HGET")
                .arg(self.keys.token_key(A::NAME, ns, h))
                .arg("spent");
        });
        let results: Vec<Option<String>> = pipe.query_async(&mut conn).await?;
        Ok(hashes
            .iter()
            .zip(results)
            .map(|(h, s)| (h.clone(), s.map(|v| v == "1")))
            .collect())
    }

    async fn batch_replace(
        &self,
        ns: &Namespace,
        ops: &[ReplaceOp<A::Record>],
    ) -> Vec<ReplaceResult> {
        if ops.is_empty() {
            return Vec::new();
        }
        let mut conn = self.conn();
        let mut pipe = redis::pipe();
        let now = chrono::Utc::now().to_rfc3339();

        ops.iter().for_each(|op| {
            let ik: Vec<String> = op
                .inputs
                .iter()
                .map(|h| self.keys.token_key(A::NAME, ns, h))
                .collect();
            let ok: Vec<String> = op
                .outputs
                .iter()
                .map(|o| self.keys.token_key(A::NAME, ns, o.public_hash()))
                .collect();
            let nk = ik.len() + ok.len();
            let audit_key = self.keys.replacement_key(A::NAME, ns, &op.record.id);
            let audit_json = serde_json::to_string(&op.record).unwrap_or_default();

            // Compute output fields (each output has the same set of fields)
            let mut sample = HashMap::new();
            if let Some(first) = op.outputs.first() {
                first.to_fields(&mut sample);
            }
            let fields_per_output = sample.len();

            let cmd = pipe.cmd("EVALSHA").arg(&self.replace_sha).arg(nk);
            ik.iter().chain(ok.iter()).for_each(|k| {
                cmd.arg(k);
            });
            cmd.arg(op.inputs.len().to_string())
                .arg(op.outputs.len().to_string())
                .arg(fields_per_output.to_string())
                .arg(&now)
                .arg(&audit_key)
                .arg(&audit_json);
            // For each output: emit the field name/value pairs in stable order.
            op.outputs.iter().for_each(|o| {
                let mut fields = HashMap::new();
                o.to_fields(&mut fields);
                let mut entries: Vec<_> = fields.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                entries.iter().for_each(|(k, v)| {
                    cmd.arg(k).arg(v);
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

    async fn batch_burn(&self, ns: &Namespace, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
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
                .arg(self.keys.token_key(A::NAME, ns, hash))
                .arg(self.keys.burn_key(A::NAME, ns, &record.id))
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
        let json: Option<String> = conn.get(self.keys.mining_state_key(A::NAME)).await?;
        json.map(|j| serde_json::from_str(&j).map_err(Into::into))
            .transpose()
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let mut conn = self.conn();
        let json = serde_json::to_string(state)?;
        let _: () = conn.set(self.keys.mining_state_key(A::NAME), &json).await?;
        Ok(())
    }
}
