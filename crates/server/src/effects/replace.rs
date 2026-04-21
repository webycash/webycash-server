//! Replace operation: pure validation + single atomic DB call.
//!
//! The replace logic separates pure computation (parsing, amount conservation)
//! from the single atomic database operation. All backends (Redis Lua, DynamoDB
//! TransactWriteItems, FDB transactions) validate inputs exist and are unspent
//! atomically — so we skip redundant pre-validation and go straight to
//! atomic_replace in ONE round-trip.

use std::str::FromStr;

use chrono::{DateTime, Utc};

use crate::db::{LedgerStore, LedgerStoreExt, ReplacementRecord, TokenOrigin, TokenRecord};
use crate::protocol::{Amount, SecretWebcash};

/// Parse and validate input webcash strings. Pure computation, no IO.
fn parse_inputs(webcashes: &[String]) -> Result<(Vec<String>, Amount), String> {
    webcashes.iter().try_fold(
        (Vec::with_capacity(webcashes.len()), Amount::ZERO),
        |(mut hashes, total), wc_str| {
            let secret = SecretWebcash::from_str(wc_str)
                .map_err(|e| format!("invalid input webcash: {e}"))?;
            if !secret.amount.is_positive() {
                return Err("input amount must be positive".into());
            }
            let new_total = total
                .checked_add(secret.amount)
                .ok_or_else(|| "input amount overflow".to_string())?;
            hashes.push(secret.to_public().hash);
            Ok((hashes, new_total))
        },
    )
}

/// Parse and validate output webcash strings. Pure computation, no IO.
fn parse_outputs(
    new_webcashes: &[String],
    now: DateTime<Utc>,
) -> Result<(Vec<TokenRecord>, Amount), String> {
    new_webcashes.iter().try_fold(
        (Vec::with_capacity(new_webcashes.len()), Amount::ZERO),
        |(mut records, total), wc_str| {
            let secret = SecretWebcash::from_str(wc_str)
                .map_err(|e| format!("invalid output webcash: {e}"))?;
            if !secret.amount.is_positive() {
                return Err("output amount must be positive".into());
            }
            let new_total = total
                .checked_add(secret.amount)
                .ok_or_else(|| "output amount overflow".to_string())?;
            let public = secret.to_public();
            records.push(TokenRecord {
                public_hash: public.hash,
                amount_wats: secret.amount.wats,
                spent: false,
                created_at: now,
                spent_at: None,
                origin: TokenOrigin::Replaced,
            });
            Ok((records, new_total))
        },
    )
}

/// Pre-validated replace data ready for batch execution.
pub struct ReplaceValidated {
    pub input_hashes: Vec<String>,
    pub output_records: Vec<TokenRecord>,
    pub record: ReplacementRecord,
}

impl ReplaceValidated {
    pub fn into_op(self) -> crate::db::ReplaceOp {
        crate::db::ReplaceOp {
            inputs: self.input_hashes,
            outputs: self.output_records,
            record: self.record,
        }
    }
}

/// Pure validation: parse tokens, check amounts, conservation law.
/// Returns validated data ready for batch execution. Zero IO.
pub fn parse_and_validate_replace(
    webcashes: Vec<String>,
    new_webcashes: Vec<String>,
) -> anyhow::Result<ReplaceValidated> {
    let (input_hashes, input_total) =
        parse_inputs(&webcashes).map_err(|e| anyhow::anyhow!("{e}"))?;

    let now = chrono::Utc::now();
    let (output_records, output_total) =
        parse_outputs(&new_webcashes, now).map_err(|e| anyhow::anyhow!("{e}"))?;

    if input_total != output_total {
        anyhow::bail!(
            "amount mismatch: inputs={} outputs={}",
            input_total,
            output_total
        );
    }
    if !input_total.is_positive() {
        anyhow::bail!("replacement must have positive amount");
    }

    let output_hashes: Vec<String> = output_records
        .iter()
        .map(|r| r.public_hash.clone())
        .collect();
    let record = ReplacementRecord {
        id: uuid::Uuid::new_v4().to_string(),
        input_hashes: input_hashes.clone(),
        output_hashes,
        total_amount_wats: input_total.wats,
        created_at: now,
    };

    Ok(ReplaceValidated {
        input_hashes,
        output_records,
        record,
    })
}

/// Execute a replace: pure validation then ONE atomic DB call.
pub async fn execute_replace(
    store: &dyn LedgerStore,
    webcashes: Vec<String>,
    new_webcashes: Vec<String>,
) -> anyhow::Result<()> {
    let v = parse_and_validate_replace(webcashes, new_webcashes)?;
    store
        .atomic_replace(&v.input_hashes, &v.output_records, &v.record)
        .await
}
