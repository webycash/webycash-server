use async_trait::async_trait;
use aws_sdk_dynamodb::types::AttributeValue;
use aws_sdk_dynamodb::Client;

use super::{BurnRecord, EconomyStats, LedgerStore, ReplacementRecord, TokenRecord};
use crate::config::DbConfig;
use crate::protocol::mining::MiningState;

const TOKENS_TABLE: &str = "WebcashTokens";
const MINING_STATE_TABLE: &str = "WebcashMiningState";
const AUDIT_TABLE: &str = "WebcashAuditLog";
const MINING_STATE_PK: &str = "current";

pub struct DynamoDbStore {
    client: Client,
    suffix: String,
}

impl DynamoDbStore {
    pub async fn new(db_config: &DbConfig) -> anyhow::Result<Self> {
        let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let mut builder = aws_sdk_dynamodb::config::Builder::from(&aws_config);

        if let Some(endpoint) = &db_config.dynamodb_endpoint {
            builder = builder.endpoint_url(endpoint);
        }

        let client = Client::from_conf(builder.build());

        let suffix = std::env::var("DEPLOYMENT_ENV")
            .map(|e| format!("-{}", e))
            .unwrap_or_else(|_| "-testnet".into());

        Ok(Self { client, suffix })
    }

    fn tokens_table(&self) -> String {
        format!("{}{}", TOKENS_TABLE, self.suffix)
    }

    fn mining_table(&self) -> String {
        format!("{}{}", MINING_STATE_TABLE, self.suffix)
    }

    fn audit_table(&self) -> String {
        format!("{}{}", AUDIT_TABLE, self.suffix)
    }

    pub async fn ensure_tables(&self) -> anyhow::Result<()> {
        use aws_sdk_dynamodb::types::{
            AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
        };

        let tables = vec![
            (
                self.tokens_table(),
                vec![("public_hash", ScalarAttributeType::S)],
                vec![("public_hash", KeyType::Hash)],
            ),
            (
                self.mining_table(),
                vec![("id", ScalarAttributeType::S)],
                vec![("id", KeyType::Hash)],
            ),
            (
                self.audit_table(),
                vec![
                    ("log_id", ScalarAttributeType::S),
                    ("created_at", ScalarAttributeType::S),
                ],
                vec![("log_id", KeyType::Hash), ("created_at", KeyType::Range)],
            ),
        ];

        for (name, attrs, keys) in tables {
            let existing = self.client.describe_table().table_name(&name).send().await;
            if existing.is_ok() {
                continue;
            }

            let attr_defs: Vec<_> = attrs
                .iter()
                .map(|(n, t)| {
                    AttributeDefinition::builder()
                        .attribute_name(*n)
                        .attribute_type(t.clone())
                        .build()
                        .map_err(|e| anyhow::anyhow!("invalid attribute definition: {}", e))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            let key_schema: Vec<_> = keys
                .iter()
                .map(|(n, t)| {
                    KeySchemaElement::builder()
                        .attribute_name(*n)
                        .key_type(t.clone())
                        .build()
                        .map_err(|e| anyhow::anyhow!("invalid key schema: {}", e))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;

            self.client
                .create_table()
                .table_name(&name)
                .set_attribute_definitions(Some(attr_defs))
                .set_key_schema(Some(key_schema))
                .billing_mode(BillingMode::PayPerRequest)
                .send()
                .await?;

            tracing::info!(table = %name, "created DynamoDB table");
        }

        Ok(())
    }
}

#[async_trait]
impl LedgerStore for DynamoDbStore {
    async fn insert_token(&self, record: &TokenRecord) -> anyhow::Result<()> {
        let json = serde_json::to_string(record)?;
        self.client
            .put_item()
            .table_name(self.tokens_table())
            .item("public_hash", AttributeValue::S(record.public_hash.clone()))
            .item("data", AttributeValue::S(json))
            .item("spent", AttributeValue::Bool(record.spent))
            .condition_expression("attribute_not_exists(public_hash)")
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("insert token failed: {}", e))?;
        Ok(())
    }

    async fn get_token(&self, public_hash: &str) -> anyhow::Result<Option<TokenRecord>> {
        let result = self
            .client
            .get_item()
            .table_name(self.tokens_table())
            .key("public_hash", AttributeValue::S(public_hash.to_string()))
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

    async fn mark_spent(&self, public_hash: &str) -> anyhow::Result<bool> {
        let token = self.get_token(public_hash).await?;
        match token {
            None => Ok(false),
            Some(t) if t.spent => Ok(false),
            Some(mut t) => {
                t.spent = true;
                t.spent_at = Some(chrono::Utc::now());
                let json = serde_json::to_string(&t)?;
                // Condition: token must still exist and not be spent (prevents TOCTOU race)
                let result = self
                    .client
                    .put_item()
                    .table_name(self.tokens_table())
                    .item("public_hash", AttributeValue::S(public_hash.to_string()))
                    .item("data", AttributeValue::S(json))
                    .item("spent", AttributeValue::Bool(true))
                    .condition_expression(
                        "attribute_exists(public_hash) AND (attribute_not_exists(spent) OR spent = :false_val)",
                    )
                    .expression_attribute_values(":false_val", AttributeValue::Bool(false))
                    .send()
                    .await;
                match result {
                    Ok(_) => Ok(true),
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("ConditionalCheckFailed") {
                            Ok(false) // Already spent by concurrent request
                        } else {
                            Err(anyhow::anyhow!("mark_spent failed: {}", err_str))
                        }
                    }
                }
            }
        }
    }

    async fn atomic_replace(
        &self,
        inputs: &[String],
        outputs: &[TokenRecord],
        record: &ReplacementRecord,
    ) -> anyhow::Result<()> {
        use aws_sdk_dynamodb::types::{Put, TransactWriteItem};

        let now = chrono::Utc::now();
        let mut items = Vec::new();

        // Mark inputs spent with condition expressions (prevents TOCTOU double-spend)
        for hash in inputs {
            let mut token = self
                .get_token(hash)
                .await?
                .ok_or_else(|| anyhow::anyhow!("input token not found: {}", hash))?;
            if token.spent {
                anyhow::bail!("input token already spent: {}", hash);
            }
            token.spent = true;
            token.spent_at = Some(now);
            let json = serde_json::to_string(&token)?;

            // Condition: token must exist AND not be spent at transaction time
            // This prevents TOCTOU: if a concurrent request spends this token
            // between our get_token() and this write, the transaction fails.
            items.push(
                TransactWriteItem::builder()
                    .put(
                        Put::builder()
                            .table_name(self.tokens_table())
                            .item("public_hash", AttributeValue::S(hash.clone()))
                            .item("data", AttributeValue::S(json))
                            .item("spent", AttributeValue::Bool(true))
                            .condition_expression(
                                "attribute_exists(public_hash) AND (attribute_not_exists(spent) OR spent = :false_val)",
                            )
                            .expression_attribute_values(":false_val", AttributeValue::Bool(false))
                            .build()?,
                    )
                    .build(),
            );
        }

        // Insert outputs
        for output in outputs {
            let json = serde_json::to_string(output)?;
            items.push(
                TransactWriteItem::builder()
                    .put(
                        Put::builder()
                            .table_name(self.tokens_table())
                            .item("public_hash", AttributeValue::S(output.public_hash.clone()))
                            .item("data", AttributeValue::S(json))
                            .condition_expression("attribute_not_exists(public_hash)")
                            .build()?,
                    )
                    .build(),
            );
        }

        // Audit record
        let audit_json = serde_json::to_string(record)?;
        items.push(
            TransactWriteItem::builder()
                .put(
                    Put::builder()
                        .table_name(self.audit_table())
                        .item("log_id", AttributeValue::S(record.id.clone()))
                        .item(
                            "created_at",
                            AttributeValue::S(record.created_at.to_rfc3339()),
                        )
                        .item("action", AttributeValue::S("replace".to_string()))
                        .item("data", AttributeValue::S(audit_json))
                        .build()?,
                )
                .build(),
        );

        self.client
            .transact_write_items()
            .set_transact_items(Some(items))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("atomic replace failed: {}", e))?;

        Ok(())
    }

    async fn get_mining_state(&self) -> anyhow::Result<Option<MiningState>> {
        let result = self
            .client
            .get_item()
            .table_name(self.mining_table())
            .key("id", AttributeValue::S(MINING_STATE_PK.to_string()))
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
            .table_name(self.mining_table())
            .item("id", AttributeValue::S(MINING_STATE_PK.to_string()))
            .item("data", AttributeValue::S(json))
            .send()
            .await?;
        Ok(())
    }

    async fn burn_token(&self, public_hash: &str, record: &BurnRecord) -> anyhow::Result<()> {
        use aws_sdk_dynamodb::types::{Put, TransactWriteItem};

        // Atomic: mark spent + write audit in single transaction
        let mut token = self
            .get_token(public_hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("token not found: {}", public_hash))?;
        if token.spent {
            anyhow::bail!("token already spent: {}", public_hash);
        }
        token.spent = true;
        token.spent_at = Some(chrono::Utc::now());
        let token_json = serde_json::to_string(&token)?;
        let audit_json = serde_json::to_string(record)?;

        let items = vec![
            // Mark token spent with condition (prevents TOCTOU)
            TransactWriteItem::builder()
                .put(
                    Put::builder()
                        .table_name(self.tokens_table())
                        .item("public_hash", AttributeValue::S(public_hash.to_string()))
                        .item("data", AttributeValue::S(token_json))
                        .item("spent", AttributeValue::Bool(true))
                        .condition_expression(
                            "attribute_exists(public_hash) AND (attribute_not_exists(spent) OR spent = :false_val)",
                        )
                        .expression_attribute_values(":false_val", AttributeValue::Bool(false))
                        .build()?,
                )
                .build(),
            // Write audit record
            TransactWriteItem::builder()
                .put(
                    Put::builder()
                        .table_name(self.audit_table())
                        .item("log_id", AttributeValue::S(record.id.clone()))
                        .item(
                            "created_at",
                            AttributeValue::S(record.burned_at.to_rfc3339()),
                        )
                        .item("action", AttributeValue::S("burn".to_string()))
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
            .map_err(|e| anyhow::anyhow!("burn transaction failed: {}", e))?;

        Ok(())
    }

    async fn check_tokens(&self, hashes: &[String]) -> anyhow::Result<Vec<(String, Option<bool>)>> {
        let mut results = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let token = self.get_token(hash).await?;
            results.push((hash.clone(), token.map(|t| t.spent)));
        }
        Ok(results)
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
