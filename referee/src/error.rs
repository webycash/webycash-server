//! Crate-wide error type.
//!
//! Every fallible operation in the referee returns `Result<T>`. The error
//! variants are deliberately coarse — fine-grained context goes in the
//! `tracing` log. The HTTP layer maps each variant to a stable status
//! code (see `docs/api.md`).

use thiserror::Error;

/// All referee-side failures.
#[derive(Debug, Error)]
pub enum RefereeError {
    /// Caller's request was malformed, missing fields, or self-contradictory.
    /// Maps to HTTP 400.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// A submitted ZKP failed verification under the configured Groth16
    /// verifier. Maps to HTTP 422.
    #[error("zkp verification failed: {0}")]
    ZkpRejected(String),

    /// MuSig2 partial-sig handling failed (nonce mismatch, malformed
    /// commitment, etc.). Maps to HTTP 422.
    #[error("musig2: {0}")]
    Musig2(String),

    /// The current state of the swap does not allow the requested
    /// transition (e.g. `/v1/swap/settle` called before ZKPs verified).
    /// Maps to HTTP 409.
    #[error("invalid state transition: {0}")]
    InvalidTransition(String),

    /// Backing store (in-memory or Postgres) failure. Maps to HTTP 500.
    #[error("store: {0}")]
    Store(String),

    /// External rail (webcash.org, RGB server, ARK ASP) returned an error
    /// or was unreachable. Maps to HTTP 502.
    #[error("external rail: {0}")]
    External(String),

    /// Push webhook delivery failed. Maps to HTTP 502.
    #[error("push: {0}")]
    Push(String),

    /// Cryptographic operation failed (signing, hashing, key handling).
    /// Maps to HTTP 500 — these are bugs, not caller errors.
    #[error("crypto: {0}")]
    Crypto(String),

    /// Catch-all for unexpected internal errors. Maps to HTTP 500.
    #[error("internal: {0}")]
    Internal(String),
}

/// `Result<T>` shorthand with [`RefereeError`] as the error type.
pub type Result<T> = std::result::Result<T, RefereeError>;

impl From<anyhow::Error> for RefereeError {
    fn from(e: anyhow::Error) -> Self {
        RefereeError::Internal(e.to_string())
    }
}

impl From<serde_json::Error> for RefereeError {
    fn from(e: serde_json::Error) -> Self {
        RefereeError::BadRequest(format!("json: {e}"))
    }
}

impl From<reqwest::Error> for RefereeError {
    fn from(e: reqwest::Error) -> Self {
        RefereeError::External(format!("http: {e}"))
    }
}
