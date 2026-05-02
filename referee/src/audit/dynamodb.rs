//! DynamoDB-backed signed audit log.
//!
//! Table layout: composite key `(swap_id, seq)`.
//!
//! | Attribute | Type | Notes |
//! |---|---|---|
//! | `swap_id` | S | partition key |
//! | `seq` | N | sort key (monotonic per swap, starting at 0) |
//! | `phase` | S | `init`, `zkps-verified`, … |
//! | `ts_unix` | N | server-stamped wall-clock |
//! | `prior_tip` | S | hex sha256 of prior entry's canonical body |
//! | `phase_payload` | S | canonical JSON |
//! | `signature` | S | hex Ed25519 signature |
//!
//! Append is implemented as a conditional `PutItem` with
//! `attribute_not_exists(swap_id)` against the next sequence number we
//! computed; on conflict, we increment and retry. Bounded retry budget
//! prevents an infinite loop under contention.

use async_trait::async_trait;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use std::collections::HashMap;

use crate::audit::{AuditEntry, AuditLog};
use crate::error::{RefereeError, Result};
use crate::sign::Identity;
use crate::state::{tag_for_phase, SwapId};

const PK: &str = "swap_id";
const SK: &str = "seq";
const APPEND_RETRIES: u32 = 8;

fn deployment_suffix() -> String {
    std::env::var("DEPLOYMENT_ENV")
        .map(|s| format!("-{s}"))
        .unwrap_or_else(|_| "-testnet".to_string())
}

/// DynamoDB-backed audit log. Construct once at boot and call
/// [`DynamoDbAuditLog::ensure_tables`] to make the underlying table.
pub struct DynamoDbAuditLog {
    client: Client,
    table: String,
}

impl DynamoDbAuditLog {
    /// New log from a constructed AWS DynamoDB client.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            table: format!("RefereeAudit{}", deployment_suffix()),
        }
    }

    /// Override the table name (tests use this to keep parallel runs
    /// isolated).
    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.table = table.into();
        self
    }

    /// Idempotent: create the table if it doesn't exist; wait until
    /// it is `ACTIVE` before returning.
    pub async fn ensure_tables(&self) -> Result<()> {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
        };

        let create = self
            .client
            .create_table()
            .table_name(&self.table)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(PK)
                    .attribute_type(ScalarAttributeType::S)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("attr def pk: {e}")))?,
            )
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(SK)
                    .attribute_type(ScalarAttributeType::N)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("attr def sk: {e}")))?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(PK)
                    .key_type(KeyType::Hash)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("key schema pk: {e}")))?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(SK)
                    .key_type(KeyType::Range)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("key schema sk: {e}")))?,
            )
            .send()
            .await;
        match create {
            Ok(_) => {}
            Err(e) if format!("{e:?}").contains("ResourceInUse") => {}
            Err(e) => return Err(RefereeError::Store(format!("create_table: {e}"))),
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let desc = self
                .client
                .describe_table()
                .table_name(&self.table)
                .send()
                .await
                .map_err(|e| RefereeError::Store(format!("describe_table: {e}")))?;
            let status = desc
                .table()
                .and_then(|t| t.table_status())
                .map(|s| s.as_str().to_string())
                .unwrap_or_default();
            if status == "ACTIVE" {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(RefereeError::Store(format!(
                    "table {} did not become ACTIVE within 30s (status={status})",
                    self.table
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        Ok(())
    }

    async fn next_seq(&self, swap_id: &SwapId) -> Result<u64> {
        // Cheap "last seq" query: sort descending, limit 1.
        let resp = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", PK)
            .expression_attribute_values(":pk", AttributeValue::S(swap_id.0.clone()))
            .scan_index_forward(false)
            .limit(1)
            .send()
            .await
            .map_err(|e| RefereeError::Store(format!("query last seq: {e}")))?;
        match resp.items().first() {
            None => Ok(0),
            Some(item) => {
                let n = item
                    .get(SK)
                    .and_then(|v| v.as_n().ok())
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0);
                Ok(n + 1)
            }
        }
    }

    async fn put_with_seq(&self, entry: &AuditEntry, seq: u64) -> Result<bool> {
        let payload_json = serde_json::to_string(&entry.phase_payload)
            .map_err(|e| RefereeError::Store(format!("encode payload: {e}")))?;
        let mut item: HashMap<String, AttributeValue> = HashMap::new();
        item.insert(PK.into(), AttributeValue::S(entry.swap_id.0.clone()));
        item.insert(SK.into(), AttributeValue::N(seq.to_string()));
        item.insert("phase".into(), AttributeValue::S(entry.phase.clone()));
        item.insert(
            "ts_unix".into(),
            AttributeValue::N(entry.ts_unix.to_string()),
        );
        item.insert(
            "prior_tip".into(),
            AttributeValue::S(entry.prior_tip.clone()),
        );
        item.insert("phase_payload".into(), AttributeValue::S(payload_json));
        item.insert(
            "signature".into(),
            AttributeValue::S(entry.signature.clone()),
        );

        let resp = self
            .client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .condition_expression("attribute_not_exists(#sk)")
            .expression_attribute_names("#sk", SK)
            .send()
            .await;
        match resp {
            Ok(_) => Ok(true),
            Err(e) if format!("{e:?}").contains("ConditionalCheckFailed") => Ok(false),
            Err(e) => Err(RefereeError::Store(format!("put_item audit: {e}"))),
        }
    }
}

#[async_trait]
impl AuditLog for DynamoDbAuditLog {
    async fn append(&self, identity: &Identity, entry: &mut AuditEntry) -> Result<String> {
        let canonical = entry.canonical_body();
        let tag = tag_for_phase(&entry.phase);
        entry.signature = identity.sign(tag, &canonical);
        let tip = entry.tip_hash();

        // Try monotonically; on contention, advance and retry.
        let mut seq = self.next_seq(&entry.swap_id).await?;
        for _ in 0..APPEND_RETRIES {
            if self.put_with_seq(entry, seq).await? {
                return Ok(tip);
            }
            seq += 1;
        }
        Err(RefereeError::Store(format!(
            "could not append audit entry after {APPEND_RETRIES} retries (high contention)"
        )))
    }

    async fn entries_for(&self, swap_id: &SwapId) -> Result<Vec<AuditEntry>> {
        let resp = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", PK)
            .expression_attribute_values(":pk", AttributeValue::S(swap_id.0.clone()))
            .scan_index_forward(true)
            .send()
            .await
            .map_err(|e| RefereeError::Store(format!("query entries: {e}")))?;
        let mut out = Vec::new();
        for item in resp.items() {
            let phase = item
                .get("phase")
                .and_then(|v| v.as_s().ok())
                .cloned()
                .unwrap_or_default();
            let ts_unix = item
                .get("ts_unix")
                .and_then(|v| v.as_n().ok())
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0);
            let prior_tip = item
                .get("prior_tip")
                .and_then(|v| v.as_s().ok())
                .cloned()
                .unwrap_or_default();
            let payload_json = item
                .get("phase_payload")
                .and_then(|v| v.as_s().ok())
                .cloned()
                .unwrap_or_else(|| "null".into());
            let phase_payload =
                serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
            let signature = item
                .get("signature")
                .and_then(|v| v.as_s().ok())
                .cloned()
                .unwrap_or_default();
            out.push(AuditEntry {
                swap_id: swap_id.clone(),
                phase,
                ts_unix,
                prior_tip,
                phase_payload,
                signature,
            });
        }
        Ok(out)
    }

    async fn tip_for(&self, swap_id: &SwapId) -> Result<String> {
        let resp = self
            .client
            .query()
            .table_name(&self.table)
            .key_condition_expression("#pk = :pk")
            .expression_attribute_names("#pk", PK)
            .expression_attribute_values(":pk", AttributeValue::S(swap_id.0.clone()))
            .scan_index_forward(false)
            .limit(1)
            .send()
            .await
            .map_err(|e| RefereeError::Store(format!("query tip: {e}")))?;
        let Some(item) = resp.items().first() else {
            return Ok(String::new());
        };
        let phase = item
            .get("phase")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .unwrap_or_default();
        let ts_unix = item
            .get("ts_unix")
            .and_then(|v| v.as_n().ok())
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        let prior_tip = item
            .get("prior_tip")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .unwrap_or_default();
        let payload_json = item
            .get("phase_payload")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .unwrap_or_else(|| "null".into());
        let phase_payload = serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
        let signature = item
            .get("signature")
            .and_then(|v| v.as_s().ok())
            .cloned()
            .unwrap_or_default();
        let entry = AuditEntry {
            swap_id: swap_id.clone(),
            phase,
            ts_unix,
            prior_tip,
            phase_payload,
            signature,
        };
        Ok(entry.tip_hash())
    }
}
