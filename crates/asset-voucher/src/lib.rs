//! Asset implementation for Vouchers — issuer-namespaced bearer credits.
//!
//! Vouchers are ALWAYS splittable. Replace enforces `(contract_id, issuer_fp)`
//! namespace. No AluVM — vouchers are a static ledger, not a contract VM.
//!
//! Implementation lands in M5, reusing the issuer-auth + partitioning + mining
//! infrastructure stabilised in M3.
