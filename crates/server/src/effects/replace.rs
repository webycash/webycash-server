//! Replace operation expressed as a Free Monad effect program.
//!
//! The replace logic is described as composable effects, then interpreted
//! against a real or mock database. This enables pure-functional testing:
//! create a mock interpreter that feeds predetermined values.

use std::str::FromStr;

use super::{atomic_replace, get_token, LedgerEffect};
use crate::db::{ReplacementRecord, TokenOrigin, TokenRecord};
use crate::protocol::{Amount, SecretWebcash};

/// Build a replace effect program from input and output webcash strings.
/// Returns a LedgerEffect that, when interpreted, validates all inputs
/// and performs the atomic replace.
pub fn build_replace_effect(
    webcashes: Vec<String>,
    new_webcashes: Vec<String>,
) -> LedgerEffect<()> {
    // Parse inputs (pure computation, no effects needed)
    let mut input_hashes = Vec::with_capacity(webcashes.len());
    let mut input_total = Amount::ZERO;

    for wc_str in &webcashes {
        let secret = match SecretWebcash::from_str(wc_str) {
            Ok(s) => s,
            Err(e) => return LedgerEffect::fail(format!("invalid input webcash: {}", e)),
        };
        if !secret.amount.is_positive() {
            return LedgerEffect::fail("input amount must be positive");
        }
        match input_total.checked_add(secret.amount) {
            Some(t) => input_total = t,
            None => return LedgerEffect::fail("input amount overflow"),
        }
        input_hashes.push(secret.to_public().hash);
    }

    let mut output_records = Vec::with_capacity(new_webcashes.len());
    let mut output_total = Amount::ZERO;
    let now = chrono::Utc::now();

    for wc_str in &new_webcashes {
        let secret = match SecretWebcash::from_str(wc_str) {
            Ok(s) => s,
            Err(e) => return LedgerEffect::fail(format!("invalid output webcash: {}", e)),
        };
        if !secret.amount.is_positive() {
            return LedgerEffect::fail("output amount must be positive");
        }
        match output_total.checked_add(secret.amount) {
            Some(t) => output_total = t,
            None => return LedgerEffect::fail("output amount overflow"),
        }
        let public = secret.to_public();
        output_records.push(TokenRecord {
            public_hash: public.hash,
            amount_wats: secret.amount.wats,
            spent: false,
            created_at: now,
            spent_at: None,
            origin: TokenOrigin::Replaced,
        });
    }

    // Amount conservation check
    if input_total != output_total {
        return LedgerEffect::fail(format!(
            "amount mismatch: inputs={} outputs={}",
            input_total, output_total
        ));
    }
    if !input_total.is_positive() {
        return LedgerEffect::fail("replacement must have positive amount");
    }

    // Build validation chain: verify each input exists and is unspent
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

    // Chain: validate each input via GetToken, then AtomicReplace
    build_validation_chain(
        input_hashes.clone(),
        0,
        input_hashes,
        output_records,
        record,
    )
}

/// Recursively build a chain of GetToken effects to validate each input,
/// followed by the AtomicReplace effect.
fn build_validation_chain(
    hashes_to_check: Vec<String>,
    idx: usize,
    all_input_hashes: Vec<String>,
    output_records: Vec<TokenRecord>,
    record: ReplacementRecord,
) -> LedgerEffect<()> {
    if idx >= hashes_to_check.len() {
        // All inputs validated — perform the atomic replace
        return atomic_replace(all_input_hashes, output_records, record);
    }

    let hash = hashes_to_check[idx].clone();
    let remaining_hashes = hashes_to_check;

    get_token(hash.clone()).bind(move |token_opt| {
        match token_opt {
            None => LedgerEffect::fail(format!("input token not found: {}", hash)),
            Some(t) if t.spent => {
                LedgerEffect::fail(format!("input token already spent: {}", hash))
            }
            Some(_) => {
                // This input is valid; check next
                build_validation_chain(
                    remaining_hashes,
                    idx + 1,
                    all_input_hashes,
                    output_records,
                    record,
                )
            }
        }
    })
}
