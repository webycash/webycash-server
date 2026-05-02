//! FoundationDB-backed swap-state store.
//!
//! Subspaces:
//! - `referee/swap/{id}` → JSON-encoded [`Transaction`].
//! - `referee/by-bob/{fp}/{ts:020}/{id}` → empty value, range-scan
//!   index for `list_by_party`.
//! - `referee/by-alice/{fp}/{ts:020}/{id}` — symmetric Alice-side
//!   index.
//!
//! Each `upsert` writes the row + both index entries inside a single
//! FDB transaction so concurrent writers cannot leave the index
//! lagging. Caller must have invoked `unsafe { foundationdb::boot() }`
//! before constructing this store; mirrors how the asset-server
//! crates do it in their `main.rs`.

use async_trait::async_trait;

use crate::error::{RefereeError, Result};
use crate::state::{PgpFingerprint, SwapId};
use crate::store::SwapStore;
use crate::transaction::{PartyRole, Transaction, TransactionSummary};

const SWAP_PREFIX: &[u8] = b"referee/swap/";
const BY_BOB_PREFIX: &[u8] = b"referee/by-bob/";
const BY_ALICE_PREFIX: &[u8] = b"referee/by-alice/";

fn swap_key(id: &SwapId) -> Vec<u8> {
    let mut k = SWAP_PREFIX.to_vec();
    k.extend_from_slice(id.0.as_bytes());
    k
}

/// 20-digit zero-padded created_at so byte-lex order matches numeric order.
fn ts_padded(ts: u64) -> String {
    format!("{:020}", ts)
}

fn index_key(prefix: &[u8], fp: &PgpFingerprint, ts: u64, id: &SwapId) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.extend_from_slice(fp.0.as_bytes());
    k.push(b'/');
    k.extend_from_slice(ts_padded(ts).as_bytes());
    k.push(b'/');
    k.extend_from_slice(id.0.as_bytes());
    k
}

fn index_subspace(prefix: &[u8], fp: &PgpFingerprint) -> (Vec<u8>, Vec<u8>) {
    let mut start = prefix.to_vec();
    start.extend_from_slice(fp.0.as_bytes());
    start.push(b'/');
    let mut end = start.clone();
    *end.last_mut().expect("non-empty") = b'0' - 1; // < anything zero-padded
                                                    // Actually want all entries within `prefix+fp+/`: end is the
                                                    // immediate successor.
    end = start.clone();
    end.push(0xff);
    (start, end)
}

/// FoundationDB-backed `SwapStore`. Construct via
/// [`FdbSwapStore::new`] AFTER calling `foundationdb::boot()`.
pub struct FdbSwapStore {
    db: foundationdb::Database,
}

impl FdbSwapStore {
    /// Open a database against the (optional) cluster file. `None`
    /// uses the platform default.
    pub fn new(cluster_file: Option<&str>) -> Result<Self> {
        let db = foundationdb::Database::new(cluster_file)
            .map_err(|e| RefereeError::Store(format!("fdb open: {e}")))?;
        Ok(Self { db })
    }
}

#[async_trait]
impl SwapStore for FdbSwapStore {
    async fn upsert(&self, tx: &Transaction) -> Result<()> {
        let json =
            serde_json::to_vec(tx).map_err(|e| RefereeError::Store(format!("encode: {e}")))?;
        let row_key = swap_key(&tx.swap_id);
        let bob_key = index_key(
            BY_BOB_PREFIX,
            &tx.bob_pgp_fp,
            tx.created_at_unix,
            &tx.swap_id,
        );
        let alice_key = index_key(
            BY_ALICE_PREFIX,
            &tx.alice_pgp_fp,
            tx.created_at_unix,
            &tx.swap_id,
        );
        self.db
            .run(|trx, _| {
                let row_key = row_key.clone();
                let bob_key = bob_key.clone();
                let alice_key = alice_key.clone();
                let json = json.clone();
                async move {
                    trx.set(&row_key, &json);
                    trx.set(&bob_key, &[]);
                    trx.set(&alice_key, &[]);
                    Ok(())
                }
            })
            .await
            .map_err(|e| RefereeError::Store(format!("fdb upsert: {e}")))?;
        Ok(())
    }

    async fn get(&self, id: &SwapId) -> Result<Option<Transaction>> {
        let key = swap_key(id);
        let raw = self
            .db
            .run(|trx, _| {
                let key = key.clone();
                async move { Ok(trx.get(&key, false).await?) }
            })
            .await
            .map_err(|e| RefereeError::Store(format!("fdb get: {e}")))?;
        match raw {
            None => Ok(None),
            Some(bytes) => {
                Ok(Some(serde_json::from_slice(&bytes).map_err(|e| {
                    RefereeError::Store(format!("decode: {e}"))
                })?))
            }
        }
    }

    async fn list_by_party(&self, fp: &PgpFingerprint) -> Result<Vec<TransactionSummary>> {
        let (bob_start, bob_end) = index_subspace(BY_BOB_PREFIX, fp);
        let (alice_start, alice_end) = index_subspace(BY_ALICE_PREFIX, fp);
        let scan = self
            .db
            .run(|trx, _| {
                let bob_start = bob_start.clone();
                let bob_end = bob_end.clone();
                let alice_start = alice_start.clone();
                let alice_end = alice_end.clone();
                async move {
                    let opts = foundationdb::RangeOption {
                        limit: Some(1000),
                        reverse: true,
                        ..foundationdb::RangeOption::from((
                            bob_start.as_slice(),
                            bob_end.as_slice(),
                        ))
                    };
                    let bob = trx.get_range(&opts, 1, false).await?;
                    let opts = foundationdb::RangeOption {
                        limit: Some(1000),
                        reverse: true,
                        ..foundationdb::RangeOption::from((
                            alice_start.as_slice(),
                            alice_end.as_slice(),
                        ))
                    };
                    let alice = trx.get_range(&opts, 1, false).await?;
                    let mut out = Vec::new();
                    for kv in bob.iter().chain(alice.iter()) {
                        let key = kv.key();
                        // The id is the trailing path segment after the
                        // final `/`.
                        if let Some(last_slash) = key.iter().rposition(|b| *b == b'/') {
                            let id_bytes = &key[last_slash + 1..];
                            if let Ok(id) = std::str::from_utf8(id_bytes) {
                                out.push(id.to_string());
                            }
                        }
                    }
                    Ok(out)
                }
            })
            .await
            .map_err(|e| RefereeError::Store(format!("fdb scan: {e}")))?;

        let mut seen = std::collections::HashSet::new();
        let mut summaries = Vec::with_capacity(scan.len());
        for id in scan {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(tx) = self.get(&SwapId(id)).await? {
                let role = if tx.bob_pgp_fp == *fp && tx.alice_pgp_fp == *fp {
                    PartyRole::Both
                } else if tx.bob_pgp_fp == *fp {
                    PartyRole::Bob
                } else {
                    PartyRole::Alice
                };
                summaries.push(tx.summary(role));
            }
        }
        summaries.sort_by(|a, b| b.created_at_unix.cmp(&a.created_at_unix));
        summaries.truncate(1000);
        Ok(summaries)
    }
}
