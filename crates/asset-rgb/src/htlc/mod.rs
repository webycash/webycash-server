//! HTLC schema for RGB-mediated cross-rail swaps.
//!
//! This module is the v1 of "real" RGB contract logic on the server-mediated
//! AluVM rail. It encodes a single, well-known schema — Hash-Time-Locked
//! Contract — that lets a webycash-server-RGB output be replaced ONLY when
//! one of two predicates holds:
//!
//! - **Claim path.** The replace request carries a witness `X` such that
//!   `sha256(X) == committed_H`, AND the new output owner matches the
//!   pre-committed `claim_owner_hash`.
//! - **Refund path.** The current server time has passed `refund_after_unix`,
//!   AND the new output owner matches the pre-committed `refund_owner_hash`.
//!
//! See `webycash-server/docs/referee-zkp-based-swap.md` for the full design — when
//! HTLCs are used, what guarantees they give per asset rail, and where the
//! limits are (webcash and voucher rails are non-conditional and don't
//! benefit from this; only the RGB server can run AluVM scripts).
//!
//! ## Why the AluVM script in v1 looks degenerate
//!
//! AluVM 0.12's base ISA is a register/control-flow machine; it doesn't
//! ship with sha256, time, or state-I/O instructions. Real RGB schemas
//! reach those via a `CoreExt` extension that the AluVM session is
//! configured with — a feature we have NOT built yet (it's part of the
//! larger M3 RGB integration that needs `rgb-core`/`rgb-std`/`bp-core`).
//!
//! Until that extension lands, the v1 contract works by:
//!
//! 1. Evaluating the HTLC predicate in [`predicate::evaluate`] — a pure,
//!    auditable, well-tested Rust function. That function performs the
//!    sha256, time, and owner-match checks.
//! 2. Configuring the AluVM with `CO` (the test-result register) set to
//!    the predicate verdict.
//! 3. Executing a minimal AluVM script — `chk CO; stop;` — that returns
//!    `Status::Ok` iff the predicate accepted.
//!
//! That's a degenerate AluVM use, but it's the right boundary for v1:
//! the RGB server's `/replace` integration calls into AluVM and respects
//! its accept/reject; future schemas with state-I/O extensions slot in
//! by replacing [`predicate::evaluate`] with bytecode that performs the
//! same logic. The wire format on the `/replace` side and the wallet's
//! WASM verifier don't change.
//!
//! Document this honestly:
//! - v1 = predicate-in-Rust + AluVM-as-gate.
//! - v2 = predicate-in-bytecode-with-RGB-Cx-extension. Same semantics.
//!
//! ## What this gives the swap protocol
//!
//! Once an RGB output is in HTLC state, the server WILL refuse any
//! `/replace` that doesn't satisfy the predicate, even if the spender
//! holds the secret. That's exactly what we needed: a way to make a
//! bearer-style RGB output behave like a Bitcoin-script HTLC, so it can
//! pair with an HTLC on the other rail (Bitcoin ARK, Bitcoin on-chain,
//! or another RGB output) for a fully cryptographic HTLC swap.

pub mod predicate;
pub mod state;
pub mod vm;

pub use predicate::{evaluate, PredicateError, PredicateResult};
pub use state::{HtlcState, HtlcWitness, LockRequest};
pub use vm::execute_predicate;
