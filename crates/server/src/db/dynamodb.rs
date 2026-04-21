//! DynamoDB backend — native attributes, BatchGetItem/BatchWriteItem, zero redundant reads.
//!
//! Tokens stored as native DynamoDB attributes (not JSON blob):
//!   public_hash (PK), amount_wats (N), spent (BOOL), created_at (S), spent_at (S), origin (S)
//!
//! Replace: single TransactWriteItems with condition expressions (no pre-read).
//! Batch reads: BatchGetItem (up to 100 per call).
//! Batch writes: BatchWriteItem (up to 25 per call).

use async_trait::async_trait;
use aws_sdk_dynamodb::types::{
    AttributeValue, KeysAndAttributes, Put, TransactWriteItem, WriteRequest,
};
use aws_sdk_dynamodb::Client;

use super::{BurnRecord, LedgerStore, ReplaceOp, ReplaceResult, TokenOrigin, TokenRecord};
use crate::config::DbConfig;
use crate::protocol::mining::MiningState;

pub struct DynamoDbStore {
    client: Client,
    tokens_table: String,
    mining_table: String,
    audit_table: String,
}

impl DynamoDbStore {
    pub async fn new(db_config: &DbConfig) -> anyhow::Result<Self> {
        let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let base_builder = aws_sdk_dynamodb::config::Builder::from(&aws_config);
        let builder = db_config
            .dynamodb_endpoint
            .as_deref()
            .map(|ep| base_builder.clone().endpoint_url(ep))
            .unwrap_or(base_builder);
        let client = Client::from_conf(builder.build());

        let suffix = std::env::var("DEPLOYMENT_ENV")
            .map(|e| format!("-{e}"))
            .unwrap_or_else(|_| "-testnet".into());

        Ok(Self {
            client,
            tokens_table: format!("WebcashTokens{suffix}"),
            mining_table: format!("WebcashMiningState{suffix}"),
            audit_table: format!("WebcashAuditLog{suffix}"),
        })
    }

    pub async fn ensure_tables(&self) -> anyhow::Result<()> {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
        };

        let tables = [
            (&self.tokens_table, "public_hash", None),
            (&self.mining_table, "id", None),
            (
                &self.audit_table,
                "log_id",
                Some(("created_at", ScalarAttributeType::S)),
            ),
        ];

        for (name, pk, range) in &tables {
            if self
                .client
                .describe_table()
                .table_name(*name)
                .send()
                .await
                .is_ok()
            {
                continue;
            }

            let pk_attr = AttributeDefinition::builder()
                .attribute_name(*pk)
                .attribute_type(ScalarAttributeType::S)
                .build()?;
            let pk_key = KeySchemaElement::builder()
                .attribute_name(*pk)
                .key_type(KeyType::Hash)
                .build()?;

            let req = self
                .client
                .create_table()
                .table_name(*name)
                .attribute_definitions(pk_attr)
                .key_schema(pk_key)
                .billing_mode(BillingMode::PayPerRequest);

            let req = match range {
                Some((rk, rt)) => req
                    .attribute_definitions(
                        AttributeDefinition::builder()
                            .attribute_name(*rk)
                            .attribute_type(rt.clone())
                            .build()?,
                    )
                    .key_schema(
                        KeySchemaElement::builder()
                            .attribute_name(*rk)
                            .key_type(KeyType::Range)
                            .build()?,
                    ),
                None => req,
            };

            req.send().await?;
            tracing::info!(table = %name, "created DynamoDB table");
        }
        Ok(())
    }

    /// Convert DynamoDB item to TokenRecord using native attributes.
    fn item_to_token(
        item: &std::collections::HashMap<String, AttributeValue>,
    ) -> Option<TokenRecord> {
        Some(TokenRecord {
            public_hash: item.get("public_hash")?.as_s().ok()?.clone(),
            amount_wats: item.get("amount_wats")?.as_n().ok()?.parse().ok()?,
            spent: item
                .get("spent")
                .and_then(|v| v.as_bool().ok())
                .copied()
                .unwrap_or(false),
            created_at: item
                .get("created_at")
                .and_then(|v| v.as_s().ok())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now),
            spent_at: item
                .get("spent_at")
                .and_then(|v| v.as_s().ok())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|d| d.with_timezone(&chrono::Utc)),
            origin: match item
                .get("origin")
                .and_then(|v| v.as_s().ok())
                .map(|s| s.as_str())
            {
                Some("replaced") => TokenOrigin::Replaced,
                _ => TokenOrigin::Mined,
            },
        })
    }
}

#[async_trait]
impl LedgerStore for DynamoDbStore {
    /// BatchWriteItem: up to 25 items per call, chunked automatically.
    async fn insert_tokens(&self, records: &[TokenRecord]) -> anyhow::Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        // DynamoDB BatchWriteItem: max 25 items per request
        let chunks: Vec<&[TokenRecord]> = records.chunks(25).collect();
        futures::future::try_join_all(chunks.iter().map(|chunk| async {
            let requests: Vec<WriteRequest> = chunk
                .iter()
                .map(|r| {
                    WriteRequest::builder()
                        .put_request(
                            aws_sdk_dynamodb::types::PutRequest::builder()
                                .item("public_hash", AttributeValue::S(r.public_hash.clone()))
                                .item("amount_wats", AttributeValue::N(r.amount_wats.to_string()))
                                .item("spent", AttributeValue::Bool(r.spent))
                                .item("created_at", AttributeValue::S(r.created_at.to_rfc3339()))
                                .item(
                                    "origin",
                                    AttributeValue::S(
                                        match r.origin {
                                            TokenOrigin::Mined => "mined",
                                            TokenOrigin::Replaced => "replaced",
                                        }
                                        .into(),
                                    ),
                                )
                                .build()
                                .expect("put request build"),
                        )
                        .build()
                })
                .collect();

            self.client
                .batch_write_item()
                .request_items(&self.tokens_table, requests)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("batch insert failed: {e}"))?;
            Ok::<_, anyhow::Error>(())
        }))
        .await?;
        Ok(())
    }

    /// BatchGetItem: up to 100 items per call.
    async fn get_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<Option<TokenRecord>>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        // DynamoDB BatchGetItem: max 100 keys per request
        let chunks: Vec<&[String]> = hashes.chunks(100).collect();
        let all_items: Vec<std::collections::HashMap<String, AttributeValue>> =
            futures::future::try_join_all(chunks.iter().map(|chunk| async {
                let keys: Vec<std::collections::HashMap<String, AttributeValue>> = chunk
                    .iter()
                    .map(|h| {
                        [("public_hash".to_string(), AttributeValue::S(h.clone()))]
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
                    .map_err(|e| anyhow::anyhow!("batch get failed: {e}"))?;

                Ok::<_, anyhow::Error>(
                    result
                        .responses
                        .unwrap_or_default()
                        .remove(&self.tokens_table)
                        .unwrap_or_default(),
                )
            }))
            .await?
            .into_iter()
            .flatten()
            .collect();

        // Build a lookup map for O(1) matching
        let item_map: std::collections::HashMap<
            String,
            &std::collections::HashMap<String, AttributeValue>,
        > = all_items
            .iter()
            .filter_map(|item| {
                item.get("public_hash")
                    .and_then(|v| v.as_s().ok())
                    .map(|h| (h.clone(), item))
            })
            .collect();

        // Return in input order
        Ok(hashes
            .iter()
            .map(|h| item_map.get(h).and_then(|item| Self::item_to_token(item)))
            .collect())
    }

    /// BatchGetItem with ProjectionExpression — only fetch spent field.
    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }

        let keys: Vec<std::collections::HashMap<String, AttributeValue>> = hashes
            .iter()
            .map(|h| {
                [("public_hash".to_string(), AttributeValue::S(h.clone()))]
                    .into_iter()
                    .collect()
            })
            .collect();

        let kaa = KeysAndAttributes::builder()
            .set_keys(Some(keys))
            .projection_expression("public_hash, spent")
            .build()?;

        let result = self
            .client
            .batch_get_item()
            .request_items(&self.tokens_table, kaa)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("batch check failed: {e}"))?;

        let items = result
            .responses
            .unwrap_or_default()
            .remove(&self.tokens_table)
            .unwrap_or_default();

        let item_map: std::collections::HashMap<String, bool> = items
            .iter()
            .filter_map(|item| {
                let hash = item.get("public_hash")?.as_s().ok()?.clone();
                let spent = item
                    .get("spent")
                    .and_then(|v| v.as_bool().ok())
                    .copied()
                    .unwrap_or(false);
                Some((hash, spent))
            })
            .collect();

        Ok(hashes
            .iter()
            .map(|h| (h.clone(), item_map.get(h).copied()))
            .collect())
    }

    /// Single TransactWriteItems — NO pre-read. Condition expressions validate atomically.
    async fn batch_replace(&self, ops: &[ReplaceOp]) -> Vec<ReplaceResult> {
        if ops.is_empty() {
            return Vec::new();
        }
        futures::future::join_all(ops.iter().map(|op| async move {
            match self.exec_replace(op).await {
                Ok(()) => ReplaceResult::Ok,
                Err(e) => ReplaceResult::Failed(e.to_string()),
            }
        }))
        .await
    }

    /// Batch burn: parallel TransactWriteItems — NO pre-read.
    async fn batch_burn(&self, ops: &[(String, BurnRecord)]) -> anyhow::Result<()> {
        futures::future::try_join_all(
            ops.iter()
                .map(|(hash, record)| async move { self.exec_burn(hash, record).await }),
        )
        .await?;
        Ok(())
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let result = self
            .client
            .get_item()
            .table_name(&self.mining_table)
            .key("id", AttributeValue::S("current".into()))
            .send()
            .await?;
        match result.item {
            None => Ok(None),
            Some(item) => {
                let data = item
                    .get("data")
                    .and_then(|v| v.as_s().ok())
                    .ok_or_else(|| anyhow::anyhow!("missing data field"))?;
                Ok(Some(serde_json::from_str(data)?))
            }
        }
    }

    async fn update_mining_state(&self, state: &MiningState) -> anyhow::Result<()> {
        let json = serde_json::to_string(state)?;
        self.client
            .put_item()
            .table_name(&self.mining_table)
            .item("id", AttributeValue::S("current".into()))
            .item("data", AttributeValue::S(json))
            .send()
            .await?;
        Ok(())
    }
}

impl DynamoDbStore {
    /// Replace: single TransactWriteItems, zero pre-reads.
    /// Condition expressions validate inputs exist + unspent atomically.
    async fn exec_replace(&self, op: &ReplaceOp) -> anyhow::Result<()> {
        let now = chrono::Utc::now();

        // Update inputs: SET spent=true — condition validates exist + unspent atomically
        let input_items: Vec<TransactWriteItem> = op
            .inputs
            .iter()
            .map(|hash| {
                TransactWriteItem::builder()
                    .update(
                        aws_sdk_dynamodb::types::Update::builder()
                            .table_name(&self.tokens_table)
                            .key("public_hash", AttributeValue::S(hash.clone()))
                            .update_expression("SET spent = :true_val, spent_at = :now")
                            .condition_expression(
                                "attribute_exists(public_hash) AND spent = :false_val",
                            )
                            .expression_attribute_values(":true_val", AttributeValue::Bool(true))
                            .expression_attribute_values(":false_val", AttributeValue::Bool(false))
                            .expression_attribute_values(
                                ":now",
                                AttributeValue::S(now.to_rfc3339()),
                            )
                            .build()
                            .expect("update build"),
                    )
                    .build()
            })
            .collect();

        // Insert outputs — condition prevents duplicate
        let output_items: Vec<TransactWriteItem> = op
            .outputs
            .iter()
            .map(|output| {
                let put = {
                    let b = Put::builder()
                        .table_name(&self.tokens_table)
                        .item("public_hash", AttributeValue::S(output.public_hash.clone()))
                        .item(
                            "amount_wats",
                            AttributeValue::N(output.amount_wats.to_string()),
                        )
                        .item("spent", AttributeValue::Bool(output.spent))
                        .item(
                            "created_at",
                            AttributeValue::S(output.created_at.to_rfc3339()),
                        )
                        .item(
                            "origin",
                            AttributeValue::S(
                                match output.origin {
                                    TokenOrigin::Mined => "mined",
                                    TokenOrigin::Replaced => "replaced",
                                }
                                .into(),
                            ),
                        )
                        .condition_expression("attribute_not_exists(public_hash)");
                    match &output.spent_at {
                        Some(sa) => b.item("spent_at", AttributeValue::S(sa.to_rfc3339())),
                        None => b,
                    }
                    .build()
                    .expect("output put build")
                };
                TransactWriteItem::builder().put(put).build()
            })
            .collect();

        // Audit
        let audit_json = serde_json::to_string(&op.record)?;
        let audit_item = TransactWriteItem::builder()
            .put(
                Put::builder()
                    .table_name(&self.audit_table)
                    .item("log_id", AttributeValue::S(op.record.id.clone()))
                    .item(
                        "created_at",
                        AttributeValue::S(op.record.created_at.to_rfc3339()),
                    )
                    .item("action", AttributeValue::S("replace".into()))
                    .item("data", AttributeValue::S(audit_json))
                    .build()?,
            )
            .build();

        let items: Vec<TransactWriteItem> = input_items
            .into_iter()
            .chain(output_items)
            .chain(std::iter::once(audit_item))
            .collect();

        self.client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("replace transaction failed: {e}"))?;

        Ok(())
    }

    /// Burn: single TransactWriteItems, zero pre-reads.
    async fn exec_burn(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        let audit_json = serde_json::to_string(record)?;

        let items = vec![
            // Update spent field — condition validates exists + unspent
            TransactWriteItem::builder()
                .update(
                    aws_sdk_dynamodb::types::Update::builder()
                        .table_name(&self.tokens_table)
                        .key("public_hash", AttributeValue::S(public_hash.to_string()))
                        .update_expression("SET spent = :true_val, spent_at = :now")
                        .condition_expression(
                            "attribute_exists(public_hash) AND spent = :false_val",
                        )
                        .expression_attribute_values(":true_val", AttributeValue::Bool(true))
                        .expression_attribute_values(":false_val", AttributeValue::Bool(false))
                        .expression_attribute_values(
                            ":now",
                            AttributeValue::S(chrono::Utc::now().to_rfc3339()),
                        )
                        .build()?,
                )
                .build(),
            // Audit
            TransactWriteItem::builder()
                .put(
                    Put::builder()
                        .table_name(&self.audit_table)
                        .item("log_id", AttributeValue::S(record.id.clone()))
                        .item(
                            "created_at",
                            AttributeValue::S(record.burned_at.to_rfc3339()),
                        )
                        .item("action", AttributeValue::S("burn".into()))
                        .item("data", AttributeValue::S(audit_json))
                        .build()?,
                )
                .build(),
        ];

        self.client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("burn transaction failed: {e}"))?;

        Ok(())
    }
}
