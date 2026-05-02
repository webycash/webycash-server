//! FoundationDB-backed signed audit log.
//!
//! Subspace: `referee/audit/{swap_id}/{seq:020}` → JSON-encoded
//! [`AuditEntry`]. Sequence is zero-padded so range scans return
//! entries in append order. The 20-digit width fits a u64 with room
//! to spare.

use async_trait::async_trait;
use foundationdb::RangeOption;

use crate::audit::{AuditEntry, AuditLog};
use crate::error::{RefereeError, Result};
use crate::sign::Identity;
use crate::state::{tag_for_phase, SwapId};

const PREFIX: &[u8] = b"referee/audit/";

fn entry_key(swap_id: &SwapId, seq: u64) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(swap_id.0.as_bytes());
    k.push(b'/');
    k.extend_from_slice(format!("{seq:020}").as_bytes());
    k
}

fn swap_prefix(swap_id: &SwapId) -> Vec<u8> {
    let mut k = PREFIX.to_vec();
    k.extend_from_slice(swap_id.0.as_bytes());
    k.push(b'/');
    k
}

fn end_prefix(start: &[u8]) -> Vec<u8> {
    let mut e = start.to_vec();
    // FoundationDB exclusive end key — append 0xFF.
    e.push(0xff);
    e
}

/// FoundationDB-backed `AuditLog`. Construct AFTER calling
/// `foundationdb::boot()`.
pub struct FdbAuditLog {
    db: foundationdb::Database,
}

impl FdbAuditLog {
    /// Open a database against the (optional) cluster file.
    pub fn new(cluster_file: Option<&str>) -> Result<Self> {
        let db = foundationdb::Database::new(cluster_file)
            .map_err(|e| RefereeError::Store(format!("fdb open: {e}")))?;
        Ok(Self { db })
    }

    async fn read_entries(&self, swap_id: &SwapId) -> Result<Vec<AuditEntry>> {
        let prefix = swap_prefix(swap_id);
        let end = end_prefix(&prefix);
        let entries = self
            .db
            .run(|trx, _| {
                let prefix = prefix.clone();
                let end = end.clone();
                async move {
                    let opt = RangeOption::from((prefix.as_slice(), end.as_slice()));
                    let kvs = trx.get_range(&opt, 1024, false).await?;
                    let mut out = Vec::with_capacity(kvs.len());
                    for kv in kvs.iter() {
                        out.push(kv.value().to_vec());
                    }
                    Ok(out)
                }
            })
            .await
            .map_err(|e| RefereeError::Store(format!("fdb get_range: {e}")))?;
        let mut out = Vec::with_capacity(entries.len());
        for bytes in entries {
            let e: AuditEntry = serde_json::from_slice(&bytes)
                .map_err(|e| RefereeError::Store(format!("decode: {e}")))?;
            out.push(e);
        }
        Ok(out)
    }
}

#[async_trait]
impl AuditLog for FdbAuditLog {
    async fn append(&self, identity: &Identity, entry: &mut AuditEntry) -> Result<String> {
        let canonical = entry.canonical_body();
        let tag = tag_for_phase(&entry.phase);
        entry.signature = identity.sign(tag, &canonical);
        let tip = entry.tip_hash();

        // Compute the next sequence number under the same transaction
        // that writes the new entry, so two concurrent writers don't
        // collide on the same seq. FDB transactions are serializable.
        let json =
            serde_json::to_vec(entry).map_err(|e| RefereeError::Store(format!("encode: {e}")))?;
        let prefix = swap_prefix(&entry.swap_id);
        let end = end_prefix(&prefix);
        let swap_id = entry.swap_id.clone();
        self.db
            .run(|trx, _| {
                let prefix = prefix.clone();
                let end = end.clone();
                let swap_id = swap_id.clone();
                let json = json.clone();
                async move {
                    let opt = RangeOption::from((prefix.as_slice(), end.as_slice()));
                    let kvs = trx.get_range(&opt, 1, true).await?; // reverse, limit 1
                    let next_seq = match kvs.iter().next() {
                        None => 0u64,
                        Some(kv) => {
                            let key = kv.key();
                            // Parse the trailing 20-digit seq.
                            let tail = &key[key.len().saturating_sub(20)..];
                            let s = std::str::from_utf8(tail).unwrap_or("0");
                            s.trim_start_matches('0')
                                .parse::<u64>()
                                .map(|n| n + 1)
                                .unwrap_or(0)
                        }
                    };
                    let key = entry_key(&swap_id, next_seq);
                    trx.set(&key, &json);
                    Ok(())
                }
            })
            .await
            .map_err(|e| RefereeError::Store(format!("fdb append: {e}")))?;
        Ok(tip)
    }

    async fn entries_for(&self, swap_id: &SwapId) -> Result<Vec<AuditEntry>> {
        self.read_entries(swap_id).await
    }

    async fn tip_for(&self, swap_id: &SwapId) -> Result<String> {
        let entries = self.read_entries(swap_id).await?;
        Ok(entries.last().map(|e| e.tip_hash()).unwrap_or_default())
    }
}
