//! HTTP API surface (Axum).
//!
//! Endpoints (all under `/v1/`):
//!
//! | Method | Path | Purpose |
//! |---|---|---|
//! | GET  | `/v1/pubkey` | Returns the referee's Ed25519 identity pubkey + MuSig2 pubshare. |
//! | POST | `/v1/swap/initiate` | Begin a swap. Body carries both parties' encrypted payloads + ZKPs + Alice's nonce commitments. Server verifies ZKPs, runs pre-check, fires insert-push, schedules post-check. |
//! | POST | `/v1/swap/{id}/ack` | Recipient wallet ack callback (insert/invalidate). Push provider posts here when the wallet has handled the push. |
//! | POST | `/v1/swap/{id}/poll` | Wallet polls for the current status (which the server also announces via push). |
//! | GET  | `/v1/swap/{id}/audit` | Read the full signed audit log for a swap. |
//!
//! Every endpoint responds with `application/json`. Errors map per
//! [`crate::error::RefereeError`].

pub mod orchestrator;
pub mod router;

pub use orchestrator::Orchestrator;
pub use router::build_router;
