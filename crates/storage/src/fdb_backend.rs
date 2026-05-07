//! FoundationDB backend for `LedgerStore<A>`.
//!
//! Uses FDB tuple-layer subspaces. Each token record is stored as a single
//! JSON-encoded key (much simpler than per-field attributes used by Redis
//! and DynamoDB; FDB transactions are serializable so atomic replace is
//! straightforward). Generic over the asset type and key strategy:
//!   - `(token, <key_strategy_token_key>)` → JSON-encoded record
//!   - `(mining, <strategy_mining_state_key>)` → JSON MiningState
//!   - `(audit, replace, <id>)` → JSON ReplacementRecord
//!   - `(audit, burn, <id>)` → JSON BurnRecord
//!
//! Caller MUST have called `foundationdb::boot()` before constructing
//! the store. The legacy `boot()` returns a guard that must outlive the
//! database; we hand it back via `FdbBoot` so binaries can hold it for
//! the program lifetime.

use std::marker::PhantomData;

use crate::asset_core::Asset;
use async_trait::async_trait;
use foundationdb::tuple::Subspace;
use foundationdb::{Database, FdbBindingError, RetryableTransaction};

use crate::storage::{
    BurnRecord, HashRecord, KeyStrategy, LedgerStore, MiningState, Namespace, ReplaceOp,
    ReplaceResult,
};

pub struct FdbStore<A: Asset, K: KeyStrategy>
where
    A::Record: HashRecord + serde::Serialize + serde::de::DeserializeOwned,
{
    db: Database,
    tokens: Subspace,
    mining: Subspace,
    audit: Subspace,
    keys: K,
    _ph: PhantomData<A>,
}

impl<A: Asset, K: KeyStrategy> FdbStore<A, K>
where
    A::Record: HashRecord + serde::Serialize + serde::de::DeserializeOwned,
{
    /// Open against a FoundationDB cluster file (or default).
    /// `foundationdb::boot()` MUST already have been called.
    pub fn new(cluster_file: Option<&str>, keys: K) -> anyhow::Result<Self> {
        let db = match cluster_file {
            Some(path) => Database::from_path(path)
                .map_err(|e| anyhow::anyhow!("FDB cluster file {}: {}", path, e))?,
            None => {
                Database::default().map_err(|e| anyhow::anyhow!("default FDB database: {}", e))?
            }
        };
        Ok(Self {
            db,
            tokens: Subspace::all().subspace(&"tokens"),
            mining: Subspace::all().subspace(&"mining"),
            audit: Subspace::all().subspace(&"audit"),
            keys,
            _ph: PhantomData,
        })
    }

    fn token_key_bytes(&self, namespace: &Namespace, public_hash: &str) -> Vec<u8> {
        let pk = self.keys.token_key(A::NAME, namespace, public_hash);
        self.tokens.pack(&pk)
    }

    fn mining_state_key_bytes(&self) -> Vec<u8> {
        self.mining.pack(&self.keys.mining_state_key(A::NAME))
    }

    fn audit_replace_key_bytes(&self, namespace: &Namespace, id: &str) -> Vec<u8> {
        let pk = self.keys.replacement_key(A::NAME, namespace, id);
        self.audit.subspace(&"replace").pack(&pk)
    }

    fn audit_burn_key_bytes(&self, namespace: &Namespace, id: &str) -> Vec<u8> {
        let pk = self.keys.burn_key(A::NAME, namespace, id);
        self.audit.subspace(&"burn").pack(&pk)
    }
}

fn fdb_err(e: FdbBindingError) -> anyhow::Error {
    anyhow::anyhow!("FoundationDB error: {:?}", e)
}

async fn trx_get_json<T: serde::de::DeserializeOwned>(
    trx: &RetryableTransaction,
    key: &[u8],
) -> Result<Option<T>, FdbBindingError> {
    let slice = trx.get(key, false).await?;
    match slice {
        None => Ok(None),
        Some(bytes) => {
            let val: T = serde_json::from_slice(bytes.as_ref())
                .map_err(|e| FdbBindingError::CustomError(Box::new(e)))?;
            Ok(Some(val))
        }
    }
}

fn trx_set_json<T: serde::Serialize>(
    trx: &RetryableTransaction,
    key: &[u8],
    value: &T,
) -> Result<(), FdbBindingError> {
    let json = serde_json::to_vec(value).map_err(|e| FdbBindingError::CustomError(Box::new(e)))?;
    trx.set(key, &json);
    Ok(())
}

#[async_trait]
impl<A: Asset, K: KeyStrategy> LedgerStore<A> for FdbStore<A, K>
where
    A::Record: HashRecord + serde::Serialize + serde::de::DeserializeOwned,
{
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        // FDB transactions can carry many writes; do them all in one batch.
        let entries: Vec<(Vec<u8>, Vec<u8>)> = records
            .iter()
            .map(|r| {
                let key = self.token_key_bytes(&r.namespace(), r.public_hash());
                let json = serde_json::to_vec(r).expect("serialize record");
                (key, json)
            })
            .collect();
        self.db
            .run(|trx, _maybe_committed| {
                let entries = entries.clone();
                async move {
                    for (key, json) in &entries {
                        let existing = trx.get(key, false).await?;
                        if existing.is_some() {
                            return Err(FdbBindingError::CustomError(
                                "token already exists".to_string().into(),
                            ));
                        }
                        trx.set(key, json);
                    }
                    Ok(())
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<Vec<u8>> = hashes.iter().map(|h| self.token_key_bytes(ns, h)).collect();
        self.db
            .run(|trx, _maybe_committed| {
                let keys = keys.clone();
                async move {
                    let mut out = Vec::with_capacity(keys.len());
                    for k in &keys {
                        out.push(trx_get_json::<A::Record>(&trx, k).await?);
                    }
                    Ok(out)
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        let tokens = self.get_tokens(ns, hashes).await?;
        Ok(hashes
            .iter()
            .zip(tokens)
            .map(|(h, t)| {
                let spent = t.map(|r| {
                    let mut f = std::collections::HashMap::new();
                    r.to_fields(&mut f);
                    f.get("spent").map(|s| s == "1").unwrap_or(false)
                });
                (h.clone(), spent)
            })
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
        // Run each op in its own FDB transaction. Could batch, but per-op
        // keeps error reporting clear and matches the legacy behavior.
        let mut out = Vec::with_capacity(ops.len());
        for op in ops {
            match self.exec_replace(ns, op).await {
                Ok(()) => out.push(ReplaceResult::Ok),
                Err(e) => out.push(ReplaceResult::Failed(e.to_string())),
            }
        }
        out
    }

    async fn batch_burn(&self, ns: &Namespace, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
        for (hash, record) in ops {
            self.exec_burn(ns, hash, record).await?;
        }
        Ok(())
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let key = self.mining_state_key_bytes();
        self.db
            .run(|trx, _maybe_committed| {
                let key = key.clone();
                async move { trx_get_json::<MiningState>(&trx, &key).await }
            })
            .await
            .map_err(fdb_err)
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let key = self.mining_state_key_bytes();
        let json = serde_json::to_vec(state)?;
        self.db
            .run(|trx, _maybe_committed| {
                let key = key.clone();
                let json = json.clone();
                async move {
                    trx.set(&key, &json);
                    Ok(())
                }
            })
            .await
            .map_err(fdb_err)
    }
}

impl<A: Asset, K: KeyStrategy> FdbStore<A, K>
where
    A::Record: HashRecord + serde::Serialize + serde::de::DeserializeOwned,
{
    async fn exec_replace(&self, ns: &Namespace, op: &ReplaceOp<A::Record>) -> anyhow::Result<()> {
        // Pre-compute keys + JSON to keep the closure light.
        let input_keys: Vec<Vec<u8>> = op
            .inputs
            .iter()
            .map(|h| self.token_key_bytes(ns, h))
            .collect();
        let output_entries: Vec<(Vec<u8>, Vec<u8>)> = op
            .outputs
            .iter()
            .map(|r| {
                let key = self.token_key_bytes(&r.namespace(), r.public_hash());
                let json = serde_json::to_vec(r).expect("serialize output");
                (key, json)
            })
            .collect();
        let audit_key = self.audit_replace_key_bytes(ns, &op.record.id);
        let audit_json = serde_json::to_vec(&op.record)?;

        self.db
            .run(|trx, _maybe_committed| {
                let input_keys = input_keys.clone();
                let output_entries = output_entries.clone();
                let audit_key = audit_key.clone();
                let audit_json = audit_json.clone();
                async move {
                    // Validate inputs: exist + unspent. Mark spent.
                    for k in &input_keys {
                        let r: Option<serde_json::Value> = trx_get_json(&trx, k).await?;
                        match r {
                            None => {
                                return Err(FdbBindingError::CustomError(
                                    "input token not found".into(),
                                ));
                            }
                            Some(v) => {
                                let already_spent =
                                    v.get("spent").and_then(|x| x.as_bool()).unwrap_or(false);
                                if already_spent {
                                    return Err(FdbBindingError::CustomError(
                                        "input token already spent".into(),
                                    ));
                                }
                                let mut updated = v.clone();
                                if let Some(obj) = updated.as_object_mut() {
                                    obj.insert("spent".into(), serde_json::Value::Bool(true));
                                    obj.insert(
                                        "spent_at".into(),
                                        serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
                                    );
                                }
                                trx_set_json(&trx, k, &updated)?;
                            }
                        }
                    }
                    // Insert outputs (must not exist).
                    for (key, json) in &output_entries {
                        let existing = trx.get(key, false).await?;
                        if existing.is_some() {
                            return Err(FdbBindingError::CustomError(
                                "output token already exists".into(),
                            ));
                        }
                        trx.set(key, json);
                    }
                    // Audit log.
                    trx.set(&audit_key, &audit_json);
                    Ok(())
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn exec_burn(
        &self,
        ns: &Namespace,
        hash: &str,
        record: &BurnRecord,
    ) -> anyhow::Result<()> {
        let key = self.token_key_bytes(ns, hash);
        let audit_key = self.audit_burn_key_bytes(ns, &record.id);
        let audit_json = serde_json::to_vec(record)?;

        self.db
            .run(|trx, _maybe_committed| {
                let key = key.clone();
                let audit_key = audit_key.clone();
                let audit_json = audit_json.clone();
                async move {
                    let r: Option<serde_json::Value> = trx_get_json(&trx, &key).await?;
                    match r {
                        None => Err(FdbBindingError::CustomError("token not found".into())),
                        Some(v) => {
                            let already_spent =
                                v.get("spent").and_then(|x| x.as_bool()).unwrap_or(false);
                            if already_spent {
                                return Err(FdbBindingError::CustomError(
                                    "token already spent".into(),
                                ));
                            }
                            let mut updated = v.clone();
                            if let Some(obj) = updated.as_object_mut() {
                                obj.insert("spent".into(), serde_json::Value::Bool(true));
                                obj.insert(
                                    "spent_at".into(),
                                    serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
                                );
                            }
                            trx_set_json(&trx, &key, &updated)?;
                            trx.set(&audit_key, &audit_json);
                            Ok(())
                        }
                    }
                }
            })
            .await
            .map_err(fdb_err)
    }
}
