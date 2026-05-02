//! Phase marker types.
//!
//! Each ZST is one phase of the swap lifecycle. `SwapState<P>` is the
//! phase-typed value; transitions are functions
//! `fn (SwapState<A>, …) -> Result<SwapState<B>>`.
//!
//! Phases form an acyclic graph:
//!
//! ```text
//!       SwapInit
//!           |
//!     verify_zkps
//!           v
//!      ZkpsVerified
//!           |
//!      pre_check
//!           v
//!       PreChecked
//!           |
//!     insert_push (≤ N retries)
//!           v
//!      InsertPushed
//!           |
//!      post_check
//!         /     \
//!     spent    unspent
//!       |        |
//!    Settled   InsertPushed (retry)
//!                 |
//!         (retries exhausted → Aborted)
//!                 |
//!         invalidate_push
//!                 |
//!            Invalidated
//!                 |
//!           refund_send
//!                 |
//!             Refunded
//! ```
//!
//! `Settled` and `Refunded` are terminal. `Aborted` is intermediate
//! (used purely as a signpost between PostChecked-failure and
//! `invalidate_push`); see [`Aborted`].

/// Sealed trait — only the phase markers below implement it.
pub trait Phase: sealed::Sealed + Send + Sync + 'static {
    /// Stable name for logs and audit-log entries.
    const NAME: &'static str;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! phase {
    ($name:ident, $tag:literal) => {
        /// See [`super`] for the phase graph.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name;
        impl sealed::Sealed for $name {}
        impl Phase for $name {
            const NAME: &'static str = $tag;
        }
    };
}

phase!(SwapInit, "init");
phase!(ZkpsVerified, "zkps-verified");
phase!(PreChecked, "pre-checked");
phase!(InsertPushed, "insert-pushed");
phase!(Settled, "settled");
phase!(Aborted, "aborted");
phase!(Invalidated, "invalidated");
phase!(Refunded, "refunded");
