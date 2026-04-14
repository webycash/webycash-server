use async_trait::async_trait;
use foundationdb::tuple::Subspace;
use foundationdb::{Database, FdbBindingError, RetryableTransaction};

use super::{BurnRecord, EconomyStats, LedgerStore, ReplacementRecord, TokenRecord};
use crate::protocol::mining::MiningState;

/// Key layout using FDB tuple-layer subspaces:
///   ("tokens", <public_hash>)   -> JSON-encoded TokenRecord
///   ("mining", "state")         -> JSON-encoded MiningState
///   ("audit", "replace", <id>)  -> JSON-encoded ReplacementRecord
///   ("audit", "burn", <id>)     -> JSON-encoded BurnRecord
pub struct FdbStore {
    db: Database,
    tokens: Subspace,
    mining: Subspace,
    audit: Subspace,
}

impl FdbStore {
    /// Create a new FdbStore. The caller must have already called `foundationdb::boot()`
    /// before constructing this store. `cluster_file` is the path to the FDB cluster file,
    /// or None to use the default.
    pub fn new(cluster_file: Option<&str>) -> anyhow::Result<Self> {
        let db = match cluster_file {
            Some(path) => Database::from_path(path)
                .map_err(|e| anyhow::anyhow!("failed to open FDB cluster file {}: {}", path, e))?,
            None => Database::default()
                .map_err(|e| anyhow::anyhow!("failed to open default FDB database: {}", e))?,
        };

        let tokens = Subspace::all().subspace(&"tokens");
        let mining = Subspace::all().subspace(&"mining");
        let audit = Subspace::all().subspace(&"audit");

        Ok(Self {
            db,
            tokens,
            mining,
            audit,
        })
    }

    fn token_key(&self, public_hash: &str) -> Vec<u8> {
        self.tokens.pack(&public_hash)
    }

    fn mining_state_key(&self) -> Vec<u8> {
        self.mining.pack(&"state")
    }

    fn audit_replace_key(&self, id: &str) -> Vec<u8> {
        self.audit.subspace(&"replace").pack(&id)
    }

    fn audit_burn_key(&self, id: &str) -> Vec<u8> {
        self.audit.subspace(&"burn").pack(&id)
    }
}

/// Convert FdbBindingError to anyhow::Error.
fn fdb_err(e: FdbBindingError) -> anyhow::Error {
    anyhow::anyhow!("FoundationDB error: {:?}", e)
}

/// Read a JSON value from a key within a transaction.
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

/// Write a JSON value to a key within a transaction.
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
impl LedgerStore for FdbStore {
    async fn insert_token(&self, record: &TokenRecord) -> anyhow::Result<()> {
        let key = self.token_key(&record.public_hash);
        let json = serde_json::to_vec(record)?;
        let hash = record.public_hash.clone();

        self.db
            .run(|trx, _maybe_committed| {
                let key = key.clone();
                let json = json.clone();
                let hash = hash.clone();
                async move {
                    // Check if key already exists (prevents duplicate tokens)
                    let existing = trx.get(&key, false).await?;
                    if existing.is_some() {
                        return Err(FdbBindingError::CustomError(
                            format!("token already exists: {}", hash).into(),
                        ));
                    }
                    trx.set(&key, &json);
                    Ok(())
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn get_token(&self, public_hash: &str) -> anyhow::Result<Option<TokenRecord>> {
        let key = self.token_key(public_hash);

        self.db
            .run(|trx, _maybe_committed| {
                let key = key.clone();
                async move { trx_get_json(&trx, &key).await }
            })
            .await
            .map_err(fdb_err)
    }

    async fn mark_spent(&self, public_hash: &str) -> anyhow::Result<bool> {
        let key = self.token_key(public_hash);

        self.db
            .run(|trx, _maybe_committed| {
                let key = key.clone();
                async move {
                    let record: Option<TokenRecord> = trx_get_json(&trx, &key).await?;
                    match record {
                        None => Ok(false),
                        Some(t) if t.spent => Ok(false),
                        Some(mut t) => {
                            t.spent = true;
                            t.spent_at = Some(chrono::Utc::now());
                            trx_set_json(&trx, &key, &t)?;
                            Ok(true)
                        }
                    }
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn atomic_replace(
        &self,
        inputs: &[String],
        outputs: &[TokenRecord],
        record: &ReplacementRecord,
    ) -> anyhow::Result<()> {
        let input_keys: Vec<(String, Vec<u8>)> = inputs
            .iter()
            .map(|h| (h.clone(), self.token_key(h)))
            .collect();
        let output_entries: Vec<(Vec<u8>, Vec<u8>)> = outputs
            .iter()
            .map(|o| {
                let key = self.token_key(&o.public_hash);
                let json = serde_json::to_vec(o).expect("serialize output token");
                (key, json)
            })
            .collect();
        let audit_key = self.audit_replace_key(&record.id);
        let audit_json = serde_json::to_vec(record)?;

        self.db
            .run(|trx, _maybe_committed| {
                let input_keys = input_keys.clone();
                let output_entries = output_entries.clone();
                let audit_key = audit_key.clone();
                let audit_json = audit_json.clone();
                async move {
                    let now = chrono::Utc::now();

                    // Verify all inputs exist and are unspent, then mark spent.
                    // FDB transaction isolation guarantees atomicity: if any input
                    // is concurrently modified, the transaction will conflict and retry.
                    for (hash, key) in &input_keys {
                        let record: Option<TokenRecord> = trx_get_json(&trx, key).await?;
                        match record {
                            None => {
                                return Err(FdbBindingError::CustomError(
                                    format!("input token not found: {}", hash).into(),
                                ));
                            }
                            Some(t) if t.spent => {
                                return Err(FdbBindingError::CustomError(
                                    format!("input token already spent: {}", hash).into(),
                                ));
                            }
                            Some(mut t) => {
                                t.spent = true;
                                t.spent_at = Some(now);
                                trx_set_json(&trx, key, &t)?;
                            }
                        }
                    }

                    // Insert all outputs (fail if any already exists)
                    for (key, json) in &output_entries {
                        let existing = trx.get(key, false).await?;
                        if existing.is_some() {
                            return Err(FdbBindingError::CustomError(
                                "output token already exists".to_string().into(),
                            ));
                        }
                        trx.set(key, json);
                    }

                    // Write audit record
                    trx.set(&audit_key, &audit_json);

                    Ok(())
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let key = self.mining_state_key();

        self.db
            .run(|trx, _maybe_committed| {
                let key = key.clone();
                async move { trx_get_json(&trx, &key).await }
            })
            .await
            .map_err(fdb_err)
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let key = self.mining_state_key();
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

    async fn burn_token(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        let token_key = self.token_key(public_hash);
        let audit_key = self.audit_burn_key(&record.id);
        let audit_json = serde_json::to_vec(record)?;
        let hash = public_hash.to_string();

        self.db
            .run(|trx, _maybe_committed| {
                let token_key = token_key.clone();
                let audit_key = audit_key.clone();
                let audit_json = audit_json.clone();
                let hash = hash.clone();
                async move {
                    let token: Option<TokenRecord> = trx_get_json(&trx, &token_key).await?;
                    match token {
                        None => {
                            return Err(FdbBindingError::CustomError(
                                format!("token not found: {}", hash).into(),
                            ));
                        }
                        Some(t) if t.spent => {
                            return Err(FdbBindingError::CustomError(
                                format!("token already spent: {}", hash).into(),
                            ));
                        }
                        Some(mut t) => {
                            t.spent = true;
                            t.spent_at = Some(chrono::Utc::now());
                            trx_set_json(&trx, &token_key, &t)?;
                        }
                    }

                    // Write audit record
                    trx.set(&audit_key, &audit_json);

                    Ok(())
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        let keys: Vec<(String, Vec<u8>)> = hashes
            .iter()
            .map(|h| (h.clone(), self.token_key(h)))
            .collect();

        self.db
            .run(|trx, _maybe_committed| {
                let keys = keys.clone();
                async move {
                    let mut results = Vec::with_capacity(keys.len());
                    for (hash, key) in &keys {
                        let record: Option<TokenRecord> = trx_get_json(&trx, key).await?;
                        results.push((hash.clone(), record.map(|t| t.spent)));
                    }
                    Ok(results)
                }
            })
            .await
            .map_err(fdb_err)
    }

    async fn get_stats(&self) -> anyhow::Result<EconomyStats> {
        let state = self.get_mining_state().await?;
        match state {
            Some(s) => Ok(EconomyStats {
                total_circulation_wats: s.total_circulation_wats,
                mining_reports_count: s.mining_reports_count,
                difficulty_target_bits: s.difficulty_target_bits,
                epoch: s.epoch,
                mining_amount_wats: s.mining_amount_wats,
                subsidy_amount_wats: s.subsidy_amount_wats,
            }),
            None => Ok(EconomyStats {
                total_circulation_wats: 0,
                mining_reports_count: 0,
                difficulty_target_bits: 0,
                epoch: 0,
                mining_amount_wats: 0,
                subsidy_amount_wats: 0,
            }),
        }
    }
}
