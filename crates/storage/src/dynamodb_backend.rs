//! DynamoDB backend for `LedgerStore<A>`.
//!
//! Generic over the asset type and the key strategy. Stores each token
//! record as a single item in `WebycashTokens{-suffix}`, with the
//! key-strategy-derived path as the partition key, and the record's
//! `HashRecord` fields exploded into native attributes (amount_wats as N,
//! everything else as S; spent as BOOL).
//!
//! Atomic replace via `TransactWriteItems` with condition expressions
//! (no pre-read). Burn via single TransactWriteItems per token. Mining
//! state stored as a JSON blob in `WebycashMiningState{-suffix}`.

use std::collections::HashMap;
use std::marker::PhantomData;

use crate::asset_core::Asset;
use async_trait::async_trait;
use aws_sdk_dynamodb::types::{AttributeValue, KeysAndAttributes, TransactWriteItem, WriteRequest};
use aws_sdk_dynamodb::Client;

use crate::storage::{
    BurnRecord, HashRecord, KeyStrategy, LedgerStore, MiningState, Namespace, ReplaceOp,
    ReplaceResult,
};

// `pk` instead of `key` — `key` is a DynamoDB reserved word and would
// require ExpressionAttributeNames in every condition. `pk` is not reserved.
const PK: &str = "pk";

/// Walks `aws_sdk_dynamodb::error::SdkError` for the underlying service-level
/// `ResourceNotFoundException`. Used to distinguish "table doesn't exist yet"
/// (recoverable — go create it) from transport / dispatch failures.
fn is_resource_not_found<E, R>(e: &aws_sdk_dynamodb::error::SdkError<E, R>) -> bool
where
    E: std::fmt::Debug,
{
    matches!(
        e,
        aws_sdk_dynamodb::error::SdkError::ServiceError(svc)
            if format!("{:?}", svc.err()).contains("ResourceNotFound")
    )
}

/// Same shape as `is_resource_not_found`, but for the `ResourceInUseException`
/// raised when another worker already created the table.
fn is_resource_in_use<E, R>(e: &aws_sdk_dynamodb::error::SdkError<E, R>) -> bool
where
    E: std::fmt::Debug,
{
    matches!(
        e,
        aws_sdk_dynamodb::error::SdkError::ServiceError(svc)
            if format!("{:?}", svc.err()).contains("ResourceInUse")
    )
}

/// DynamoDB-backed `LedgerStore`. Tables are suffixed by `DEPLOYMENT_ENV`
/// (defaults to `-testnet`) so prod and testnet share an account safely.
pub struct DynamoDbStore<A: Asset, K: KeyStrategy> {
    client: Client,
    tokens_table: String,
    mining_table: String,
    audit_table: String,
    keys: K,
    _ph: PhantomData<A>,
}

impl<A: Asset, K: KeyStrategy> DynamoDbStore<A, K> {
    /// Build with a pre-configured `aws_sdk_dynamodb::Client`. The caller
    /// passes endpoint URL via the SDK config (production: real DynamoDB;
    /// integration tests: DynamoDB Local at e.g. http://localhost:8000).
    pub fn new(client: Client, keys: K) -> Self {
        let suffix = std::env::var("DEPLOYMENT_ENV")
            .map(|e| format!("-{e}"))
            .unwrap_or_else(|_| "-testnet".into());
        Self {
            client,
            tokens_table: format!("WebycashTokens{suffix}"),
            mining_table: format!("WebycashMiningState{suffix}"),
            audit_table: format!("WebycashAuditLog{suffix}"),
            keys,
            _ph: PhantomData,
        }
    }

    /// Create any missing tables (tokens / mining / audit) with on-demand
    /// billing. Idempotent — existing tables are left as-is.
    pub async fn ensure_tables(&self) -> anyhow::Result<()> {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
        };
        // Boot-time backoff: in compose stacks the DynamoDB Local container can
        // be a few hundred ms behind us starting up. Retry the whole sequence a
        // small number of times before giving up.
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut transient_err: Option<anyhow::Error> = None;
            for name in [&self.tokens_table, &self.mining_table, &self.audit_table] {
                match self.client.describe_table().table_name(name).send().await {
                    Ok(_) => continue,
                    Err(e) => {
                        // Distinguish "table doesn't exist yet" (proceed to
                        // create) from connection errors (retry the whole
                        // sequence after a short backoff).
                        if !is_resource_not_found(&e) {
                            transient_err =
                                Some(anyhow::Error::msg(format!("describe_table {name}: {e}")));
                            break;
                        }
                    }
                }
                let attr = AttributeDefinition::builder()
                    .attribute_name(PK)
                    .attribute_type(ScalarAttributeType::S)
                    .build()?;
                let key = KeySchemaElement::builder()
                    .attribute_name(PK)
                    .key_type(KeyType::Hash)
                    .build()?;
                match self
                    .client
                    .create_table()
                    .table_name(name)
                    .attribute_definitions(attr)
                    .key_schema(key)
                    .billing_mode(BillingMode::PayPerRequest)
                    .send()
                    .await
                {
                    Ok(_) => tracing::info!(table = %name, "created DynamoDB table"),
                    Err(e) => {
                        // Race with another worker: idempotent — another
                        // instance (parallel replica, sibling server flavor
                        // sharing the same DynamoDB Local in compose) created
                        // the table after our describe_table check but before
                        // our create_table.
                        if is_resource_in_use(&e) {
                            tracing::debug!(table = %name, "table already exists (race)");
                        } else {
                            transient_err =
                                Some(anyhow::Error::msg(format!("create_table {name}: {e}")));
                            break;
                        }
                    }
                }
            }
            match transient_err {
                None => return Ok(()),
                Some(e) if attempt >= 10 => return Err(e),
                Some(_) => {
                    let backoff = std::time::Duration::from_millis(200u64 * (1 << attempt.min(5)));
                    tracing::warn!(
                        ?attempt,
                        ?backoff,
                        "ensure_tables transient error, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    fn item_for(&self, namespace: &Namespace, record: &A::Record) -> HashMap<String, AttributeValue>
    where
        A::Record: HashRecord,
    {
        let pk = self
            .keys
            .token_key(A::NAME, namespace, record.public_hash());
        let mut item = HashMap::new();
        item.insert(PK.into(), AttributeValue::S(pk));
        let mut fields = HashMap::new();
        record.to_fields(&mut fields);
        for (k, v) in fields {
            // amount_wats is numeric; spent is a "0"/"1" string we
            // convert to BOOL for native compare. Everything else string.
            let attr = match k.as_str() {
                "amount_wats" => AttributeValue::N(v),
                "spent" => AttributeValue::Bool(v == "1"),
                _ => AttributeValue::S(v),
            };
            item.insert(k, attr);
        }
        item
    }

    fn item_to_record(item: &HashMap<String, AttributeValue>) -> Option<A::Record>
    where
        A::Record: HashRecord,
    {
        // Reconstruct field map for HashRecord::from_fields. The PK encodes
        // the namespace + public_hash; we extract just the hash by parsing
        // the key suffix after the last `:token:` segment.
        let pk = item.get(PK)?.as_s().ok()?;
        let public_hash = pk
            .rsplit(":token:")
            .next()
            .map(|s| s.to_string())
            .unwrap_or_else(|| pk.rsplit(':').next().unwrap_or(pk).to_string());
        let mut fields = HashMap::new();
        for (k, v) in item {
            if k == PK {
                continue;
            }
            let s = match v {
                AttributeValue::S(s) => s.clone(),
                AttributeValue::N(n) => n.clone(),
                AttributeValue::Bool(b) => if *b { "1" } else { "0" }.into(),
                _ => continue,
            };
            fields.insert(k.clone(), s);
        }
        A::Record::from_fields(&public_hash, &fields)
    }
}

#[async_trait]
impl<A: Asset, K: KeyStrategy> LedgerStore<A> for DynamoDbStore<A, K>
where
    A::Record: HashRecord + serde::Serialize + serde::de::DeserializeOwned,
{
    async fn insert_tokens(&self, records: &[A::Record]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        // BatchWriteItem max 25 per request; chunk and run sequentially
        // (parallel won't help DynamoDB Local much and complicates retries).
        for chunk in records.chunks(25) {
            let requests: Vec<WriteRequest> = chunk
                .iter()
                .map(|r| {
                    WriteRequest::builder()
                        .put_request(
                            aws_sdk_dynamodb::types::PutRequest::builder()
                                .set_item(Some(self.item_for(&r.namespace(), r)))
                                .build()
                                .expect("put request"),
                        )
                        .build()
                })
                .collect();
            self.client
                .batch_write_item()
                .request_items(&self.tokens_table, requests)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("batch insert: {e}"))?;
        }
        Ok(())
    }

    async fn get_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<Option<A::Record>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<HashMap<String, AttributeValue>> = hashes
            .iter()
            .map(|h| {
                let pk = self.keys.token_key(A::NAME, ns, h);
                [(PK.to_string(), AttributeValue::S(pk))]
                    .into_iter()
                    .collect()
            })
            .collect();
        let kaa = KeysAndAttributes::builder().set_keys(Some(keys)).build()?;
        let result = self
            .client
            .batch_get_item()
            .request_items(&self.tokens_table, kaa)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("batch get: {e}"))?;
        let items = result
            .responses
            .unwrap_or_default()
            .remove(&self.tokens_table)
            .unwrap_or_default();
        let by_pk: HashMap<String, &HashMap<String, AttributeValue>> = items
            .iter()
            .filter_map(|item| {
                item.get(PK)
                    .and_then(|v| v.as_s().ok())
                    .map(|s| (s.clone(), item))
            })
            .collect();
        Ok(hashes
            .iter()
            .map(|h| {
                let pk = self.keys.token_key(A::NAME, ns, h);
                by_pk.get(&pk).and_then(|item| Self::item_to_record(item))
            })
            .collect())
    }

    async fn check_tokens(
        &self,
        ns: &Namespace,
        hashes: &[String],
    ) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        // Reuse get_tokens; we only need the spent flag.
        let tokens = self.get_tokens(ns, hashes).await?;
        Ok(hashes
            .iter()
            .zip(tokens)
            .map(|(h, t)| {
                // We need to surface spent=Some(false) for unspent records and
                // Some(true) for spent records. HashRecord doesn't expose
                // `spent` directly; we round-trip via to_fields.
                let spent = t.map(|r| {
                    let mut f = HashMap::new();
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
        let result = self
            .client
            .get_item()
            .table_name(&self.mining_table)
            .key(PK, AttributeValue::S(self.keys.mining_state_key(A::NAME)))
            .send()
            .await?;
        match result.item {
            None => Ok(None),
            Some(item) => {
                let data = item
                    .get("data")
                    .and_then(|v| v.as_s().ok())
                    .ok_or_else(|| anyhow::anyhow!("missing data"))?;
                Ok(Some(serde_json::from_str(data)?))
            }
        }
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let json = serde_json::to_string(state)?;
        self.client
            .put_item()
            .table_name(&self.mining_table)
            .item(PK, AttributeValue::S(self.keys.mining_state_key(A::NAME)))
            .item("data", AttributeValue::S(json))
            .send()
            .await?;
        Ok(())
    }
}

impl<A: Asset, K: KeyStrategy> DynamoDbStore<A, K>
where
    A::Record: HashRecord,
{
    async fn exec_replace(&self, ns: &Namespace, op: &ReplaceOp<A::Record>) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        // Update inputs (SET spent=true, condition spent=false).
        let mut tx_items: Vec<TransactWriteItem> = op
            .inputs
            .iter()
            .map(|hash| {
                let pk = self.keys.token_key(A::NAME, ns, hash);
                TransactWriteItem::builder()
                    .update(
                        aws_sdk_dynamodb::types::Update::builder()
                            .table_name(&self.tokens_table)
                            .key(PK, AttributeValue::S(pk))
                            .update_expression("SET spent = :t, spent_at = :now")
                            .condition_expression(format!("attribute_exists({PK}) AND spent = :f"))
                            .expression_attribute_values(":t", AttributeValue::Bool(true))
                            .expression_attribute_values(":f", AttributeValue::Bool(false))
                            .expression_attribute_values(":now", AttributeValue::S(now.clone()))
                            .build()
                            .expect("update item"),
                    )
                    .build()
            })
            .collect();
        // Put outputs (condition the PK does NOT exist).
        for r in &op.outputs {
            tx_items.push(
                TransactWriteItem::builder()
                    .put(
                        aws_sdk_dynamodb::types::Put::builder()
                            .table_name(&self.tokens_table)
                            .set_item(Some(self.item_for(ns, r)))
                            .condition_expression(format!("attribute_not_exists({PK})"))
                            .build()
                            .expect("put item"),
                    )
                    .build(),
            );
        }
        // Audit log.
        let audit_pk = self.keys.replacement_key(A::NAME, ns, &op.record.id);
        tx_items.push(
            TransactWriteItem::builder()
                .put(
                    aws_sdk_dynamodb::types::Put::builder()
                        .table_name(&self.audit_table)
                        .item(PK, AttributeValue::S(audit_pk))
                        .item(
                            "data",
                            AttributeValue::S(serde_json::to_string(&op.record)?),
                        )
                        .build()
                        .expect("put audit"),
                )
                .build(),
        );
        self.client
            .transact_write_items()
            .set_transact_items(Some(tx_items))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("transact write: {e}"))?;
        Ok(())
    }

    async fn exec_burn(
        &self,
        ns: &Namespace,
        hash: &str,
        record: &BurnRecord,
    ) -> anyhow::Result<()> {
        let pk = self.keys.token_key(A::NAME, ns, hash);
        let now = chrono::Utc::now().to_rfc3339();
        let burn_pk = self.keys.burn_key(A::NAME, ns, &record.id);
        let tx = vec![
            TransactWriteItem::builder()
                .update(
                    aws_sdk_dynamodb::types::Update::builder()
                        .table_name(&self.tokens_table)
                        .key(PK, AttributeValue::S(pk))
                        .update_expression("SET spent = :t, spent_at = :now")
                        .condition_expression(format!("attribute_exists({PK}) AND spent = :f"))
                        .expression_attribute_values(":t", AttributeValue::Bool(true))
                        .expression_attribute_values(":f", AttributeValue::Bool(false))
                        .expression_attribute_values(":now", AttributeValue::S(now))
                        .build()
                        .expect("update"),
                )
                .build(),
            TransactWriteItem::builder()
                .put(
                    aws_sdk_dynamodb::types::Put::builder()
                        .table_name(&self.audit_table)
                        .item(PK, AttributeValue::S(burn_pk))
                        .item("data", AttributeValue::S(serde_json::to_string(record)?))
                        .build()
                        .expect("put burn audit"),
                )
                .build(),
        ];
        self.client
            .transact_write_items()
            .set_transact_items(Some(tx))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("burn: {e}"))?;
        Ok(())
    }
}
