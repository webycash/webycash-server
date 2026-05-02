//! DynamoDB-backed swap-state store.
//!
//! Table layout (one item per swap):
//!
//! | Attribute | Type | Notes |
//! |---|---|---|
//! | `pk` | S | swap id (partition key) |
//! | `status` | S | `pending` / `settled` / `refunded` / `canceled` |
//! | `phase` | S | detailed typestate phase |
//! | `bob_pgp_fp` | S | GSI1 hash key (`byBob`) |
//! | `alice_pgp_fp` | S | GSI2 hash key (`byAlice`) |
//! | `created_at_unix` | N | GSI sort key on both `byBob` and `byAlice` |
//! | `updated_at_unix` | N | wall-clock at last write |
//! | `terminal` | BOOL | mirrors status.is_terminal() |
//! | `webcash_public_hash` | S | top-level for inspection |
//! | `vtxo_outpoint_hash` | S | top-level for inspection |
//! | `tx_settle_hash` | S | top-level for inspection |
//! | `tx_refund_hash` | S | top-level for inspection |
//! | `insert_push_attempts` | N | top-level for inspection |
//! | `cancel_reason` | S (optional) | populated when status=canceled |
//! | `canceled_by_pgp_fp` | S (optional) | populated when status=canceled |
//! | `htlc_refund_contract_id` | S (optional) | populated at initiate |
//! | `state_blob` | S | canonical JSON of `SwapState<P>` for orchestrator continuation |
//!
//! `pk` instead of `key` because `key` is a DynamoDB reserved word.
//!
//! The table name is `RefereeSwaps{-suffix}` where `-suffix` is set
//! from `DEPLOYMENT_ENV` (defaults to `-testnet`) so prod and testnet
//! can share a single AWS account safely.

use async_trait::async_trait;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;
use std::collections::HashMap;

use crate::error::{RefereeError, Result};
use crate::state::{AnyPhaseSwapState, ArkOutpointHash, PgpFingerprint, SwapId, WebcashPublicHash};
use crate::store::SwapStore;
use crate::transaction::{PartyRole, Transaction, TransactionStatus, TransactionSummary};

const PK: &str = "pk";
const GSI_BOB: &str = "byBob";
const GSI_ALICE: &str = "byAlice";

fn deployment_suffix() -> String {
    std::env::var("DEPLOYMENT_ENV")
        .map(|s| format!("-{s}"))
        .unwrap_or_else(|_| "-testnet".to_string())
}

/// DynamoDB-backed `SwapStore`. Construct via [`DynamoDbSwapStore::new`]
/// and call [`DynamoDbSwapStore::ensure_tables`] at boot.
pub struct DynamoDbSwapStore {
    client: Client,
    table: String,
}

impl DynamoDbSwapStore {
    /// New store from a constructed AWS DynamoDB client.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            table: format!("RefereeSwaps{}", deployment_suffix()),
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
            AttributeDefinition, BillingMode, GlobalSecondaryIndex, KeySchemaElement, KeyType,
            Projection, ProjectionType, ScalarAttributeType,
        };

        let attrs = vec![
            AttributeDefinition::builder()
                .attribute_name(PK)
                .attribute_type(ScalarAttributeType::S)
                .build()
                .map_err(|e| RefereeError::Store(format!("attr def pk: {e}")))?,
            AttributeDefinition::builder()
                .attribute_name("bob_pgp_fp")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .map_err(|e| RefereeError::Store(format!("attr def bob: {e}")))?,
            AttributeDefinition::builder()
                .attribute_name("alice_pgp_fp")
                .attribute_type(ScalarAttributeType::S)
                .build()
                .map_err(|e| RefereeError::Store(format!("attr def alice: {e}")))?,
            AttributeDefinition::builder()
                .attribute_name("created_at_unix")
                .attribute_type(ScalarAttributeType::N)
                .build()
                .map_err(|e| RefereeError::Store(format!("attr def ts: {e}")))?,
        ];

        let by_bob = GlobalSecondaryIndex::builder()
            .index_name(GSI_BOB)
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("bob_pgp_fp")
                    .key_type(KeyType::Hash)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("gsi bob hash: {e}")))?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("created_at_unix")
                    .key_type(KeyType::Range)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("gsi bob range: {e}")))?,
            )
            .projection(
                Projection::builder()
                    .projection_type(ProjectionType::All)
                    .build(),
            )
            .build()
            .map_err(|e| RefereeError::Store(format!("gsi bob: {e}")))?;
        let by_alice = GlobalSecondaryIndex::builder()
            .index_name(GSI_ALICE)
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("alice_pgp_fp")
                    .key_type(KeyType::Hash)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("gsi alice hash: {e}")))?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name("created_at_unix")
                    .key_type(KeyType::Range)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("gsi alice range: {e}")))?,
            )
            .projection(
                Projection::builder()
                    .projection_type(ProjectionType::All)
                    .build(),
            )
            .build()
            .map_err(|e| RefereeError::Store(format!("gsi alice: {e}")))?;

        let mut create = self
            .client
            .create_table()
            .table_name(&self.table)
            .billing_mode(BillingMode::PayPerRequest);
        for a in attrs {
            create = create.attribute_definitions(a);
        }
        let create = create
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(PK)
                    .key_type(KeyType::Hash)
                    .build()
                    .map_err(|e| RefereeError::Store(format!("key schema: {e}")))?,
            )
            .global_secondary_indexes(by_bob)
            .global_secondary_indexes(by_alice)
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
}

fn opt_s(v: &Option<String>) -> Option<AttributeValue> {
    v.clone().map(AttributeValue::S)
}

fn opt_fp(v: &Option<PgpFingerprint>) -> Option<AttributeValue> {
    v.clone().map(|f| AttributeValue::S(f.0))
}

fn item_for(tx: &Transaction) -> Result<HashMap<String, AttributeValue>> {
    let inner = serde_json::to_string(&tx.state_blob.inner)
        .map_err(|e| RefereeError::Store(format!("encode state_blob: {e}")))?;
    let mut item: HashMap<String, AttributeValue> = HashMap::new();
    item.insert(PK.into(), AttributeValue::S(tx.swap_id.0.clone()));
    item.insert(
        "status".into(),
        AttributeValue::S(tx.status.as_str().into()),
    );
    item.insert("phase".into(), AttributeValue::S(tx.phase.clone()));
    item.insert("terminal".into(), AttributeValue::Bool(tx.terminal));
    item.insert(
        "bob_pgp_fp".into(),
        AttributeValue::S(tx.bob_pgp_fp.0.clone()),
    );
    item.insert(
        "alice_pgp_fp".into(),
        AttributeValue::S(tx.alice_pgp_fp.0.clone()),
    );
    item.insert(
        "webcash_public_hash".into(),
        AttributeValue::S(tx.webcash_public_hash.0.clone()),
    );
    item.insert(
        "vtxo_outpoint_hash".into(),
        AttributeValue::S(tx.vtxo_outpoint_hash.0.clone()),
    );
    item.insert(
        "tx_settle_hash".into(),
        AttributeValue::S(tx.tx_settle_hash.clone()),
    );
    item.insert(
        "tx_refund_hash".into(),
        AttributeValue::S(tx.tx_refund_hash.clone()),
    );
    item.insert(
        "created_at_unix".into(),
        AttributeValue::N(tx.created_at_unix.to_string()),
    );
    item.insert(
        "updated_at_unix".into(),
        AttributeValue::N(tx.updated_at_unix.to_string()),
    );
    item.insert(
        "insert_push_attempts".into(),
        AttributeValue::N(tx.insert_push_attempts.to_string()),
    );
    if let Some(v) = opt_s(&tx.cancel_reason) {
        item.insert("cancel_reason".into(), v);
    }
    if let Some(v) = opt_fp(&tx.canceled_by_pgp_fp) {
        item.insert("canceled_by_pgp_fp".into(), v);
    }
    if let Some(v) = opt_s(&tx.htlc_refund_contract_id) {
        item.insert("htlc_refund_contract_id".into(), v);
    }
    item.insert("state_blob".into(), AttributeValue::S(inner));
    Ok(item)
}

fn read_string(item: &HashMap<String, AttributeValue>, k: &str) -> Result<String> {
    item.get(k)
        .and_then(|v| v.as_s().ok())
        .cloned()
        .ok_or_else(|| RefereeError::Store(format!("missing attr {k}")))
}

fn read_optional_string(item: &HashMap<String, AttributeValue>, k: &str) -> Option<String> {
    item.get(k).and_then(|v| v.as_s().ok()).cloned()
}

fn read_u64(item: &HashMap<String, AttributeValue>, k: &str) -> u64 {
    item.get(k)
        .and_then(|v| v.as_n().ok())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

fn item_to_transaction(item: HashMap<String, AttributeValue>) -> Result<Transaction> {
    let phase = read_string(&item, "phase")?;
    let status_str = read_string(&item, "status").unwrap_or_else(|_| "pending".into());
    let status = match status_str.as_str() {
        "settled" => TransactionStatus::Settled,
        "refunded" => TransactionStatus::Refunded,
        "canceled" => TransactionStatus::Canceled,
        _ => TransactionStatus::Pending,
    };
    let state_blob_json = read_string(&item, "state_blob").unwrap_or_else(|_| "null".into());
    let inner = serde_json::from_str(&state_blob_json)
        .map_err(|e| RefereeError::Store(format!("decode state_blob: {e}")))?;
    Ok(Transaction {
        swap_id: SwapId(read_string(&item, PK)?),
        status,
        phase: phase.clone(),
        terminal: status.is_terminal(),
        bob_pgp_fp: PgpFingerprint(read_string(&item, "bob_pgp_fp")?),
        alice_pgp_fp: PgpFingerprint(read_string(&item, "alice_pgp_fp")?),
        webcash_public_hash: WebcashPublicHash::new(
            read_string(&item, "webcash_public_hash").unwrap_or_default(),
        ),
        vtxo_outpoint_hash: ArkOutpointHash(
            read_string(&item, "vtxo_outpoint_hash").unwrap_or_default(),
        ),
        tx_settle_hash: read_string(&item, "tx_settle_hash").unwrap_or_default(),
        tx_refund_hash: read_string(&item, "tx_refund_hash").unwrap_or_default(),
        created_at_unix: read_u64(&item, "created_at_unix"),
        updated_at_unix: read_u64(&item, "updated_at_unix"),
        insert_push_attempts: read_u64(&item, "insert_push_attempts") as u8,
        cancel_reason: read_optional_string(&item, "cancel_reason"),
        canceled_by_pgp_fp: read_optional_string(&item, "canceled_by_pgp_fp").map(PgpFingerprint),
        htlc_refund_contract_id: read_optional_string(&item, "htlc_refund_contract_id"),
        state_blob: AnyPhaseSwapState { phase, inner },
    })
}

#[async_trait]
impl SwapStore for DynamoDbSwapStore {
    async fn upsert(&self, tx: &Transaction) -> Result<()> {
        let item = item_for(tx)?;
        self.client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .send()
            .await
            .map_err(|e| RefereeError::Store(format!("put_item: {e}")))?;
        Ok(())
    }

    async fn get(&self, id: &SwapId) -> Result<Option<Transaction>> {
        let resp = self
            .client
            .get_item()
            .table_name(&self.table)
            .key(PK, AttributeValue::S(id.0.clone()))
            .consistent_read(true)
            .send()
            .await
            .map_err(|e| RefereeError::Store(format!("get_item: {e}")))?;
        let Some(item) = resp.item else {
            return Ok(None);
        };
        Ok(Some(item_to_transaction(item)?))
    }

    async fn list_by_party(&self, fp: &PgpFingerprint) -> Result<Vec<TransactionSummary>> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (index, role_when_match) in [(GSI_BOB, PartyRole::Bob), (GSI_ALICE, PartyRole::Alice)] {
            let attr_name = if index == GSI_BOB {
                "bob_pgp_fp"
            } else {
                "alice_pgp_fp"
            };
            let resp = self
                .client
                .query()
                .table_name(&self.table)
                .index_name(index)
                .key_condition_expression("#fp = :fp")
                .expression_attribute_names("#fp", attr_name)
                .expression_attribute_values(":fp", AttributeValue::S(fp.0.clone()))
                .scan_index_forward(false)
                .limit(1000)
                .send()
                .await
                .map_err(|e| RefereeError::Store(format!("query {index}: {e}")))?;
            for item in resp.items.unwrap_or_default() {
                let tx = item_to_transaction(item)?;
                if !seen.insert(tx.swap_id.0.clone()) {
                    continue;
                }
                let role = if tx.bob_pgp_fp == *fp && tx.alice_pgp_fp == *fp {
                    PartyRole::Both
                } else {
                    role_when_match
                };
                out.push(tx.summary(role));
            }
        }
        out.sort_by(|a, b| b.created_at_unix.cmp(&a.created_at_unix));
        out.truncate(1000);
        Ok(out)
    }

    async fn mark_terminal(&self, id: &SwapId) -> Result<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key(PK, AttributeValue::S(id.0.clone()))
            .update_expression("SET terminal = :t")
            .expression_attribute_values(":t", AttributeValue::Bool(true))
            .send()
            .await
            .map_err(|e| RefereeError::Store(format!("mark_terminal: {e}")))?;
        Ok(())
    }
}
