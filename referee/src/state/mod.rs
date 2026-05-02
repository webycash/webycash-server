//! Phase-typed `SwapState<P>` typestate. Pure transitions.
//!
//! The single most important property of this module: **transitions are
//! pure functions, not method calls on a mutable struct**. A swap is
//! represented as a sequence of immutable values
//! `SwapState<SwapInit> → SwapState<ZkpsVerified> → … → SwapState<Settled>`,
//! each constructed from the prior by a transition function that *consumes*
//! the prior value. This rules out at compile time any sequence of
//! operations that violates the protocol — you cannot call `post_check`
//! on a `SwapState<SwapInit>` because that function only accepts
//! `SwapState<InsertPushed>`.
//!
//! ## Why typestate
//!
//! The protocol has cryptographic invariants that are hard to enforce
//! at runtime alone:
//!
//! - `pre_check` must run before `insert_push` (can't push without
//!   knowing webcash leg is unspent).
//! - `post_check` must run *after* `insert_push` (we need to detect
//!   whether Alice's `/replace` landed).
//! - Settlement (release Alice's encrypted-to-Bob signature) requires
//!   the post-check to have observed `H_B` spent.
//!
//! Phantom-typed phases turn each invariant into a compile error rather
//! than a runtime panic.
//!
//! ## What an immutable value carries
//!
//! Every `SwapState<P>` carries the same payload (parties, ciphertexts,
//! ZKPs, configured timeouts, audit chain hash) but differs in the phase
//! marker. Transitions produce new values; the prior is moved (consumed)
//! and may not be reused.
//!
//! See `docs/architecture.md` and `docs/referee-zkp-based-swap.md §10`.

pub mod phases;
pub mod transitions;
pub mod types;

pub use phases::*;
pub use transitions::*;
pub use types::*;
