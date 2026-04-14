pub mod interpreter;
pub mod replace;

use crate::db::{ReplacementRecord, TokenRecord};
use crate::protocol::mining::MiningState;

/// A ledger effect — a description of a side-effectful operation.
/// Composed into programs via `bind()`, then interpreted against
/// a real or mock database backend.
///
/// The Free Monad pattern: effects are DATA describing what to do,
/// not imperative code. This enables:
/// - Pure-functional testing with mock interpreters
/// - Composable operation pipelines via bind()
/// - Separation of "what" from "how"
pub enum LedgerEffect<A: Send + 'static> {
    /// Pure value — no side effect.
    Pure(A),
    /// Look up a token by public hash.
    GetToken {
        hash: String,
        next: Box<dyn FnOnce(Option<TokenRecord>) -> LedgerEffect<A> + Send>,
    },
    /// Insert a new token.
    InsertToken {
        record: TokenRecord,
        next: Box<dyn FnOnce(()) -> LedgerEffect<A> + Send>,
    },
    /// Mark a token as spent.
    MarkSpent {
        hash: String,
        next: Box<dyn FnOnce(bool) -> LedgerEffect<A> + Send>,
    },
    /// Get current mining state.
    GetMiningState {
        next: Box<dyn FnOnce(Option<MiningState>) -> LedgerEffect<A> + Send>,
    },
    /// Update mining state.
    UpdateMiningState {
        state: MiningState,
        next: Box<dyn FnOnce(()) -> LedgerEffect<A> + Send>,
    },
    /// Atomic replace: mark inputs spent + insert outputs + write audit.
    /// The entire operation succeeds or fails — no partial writes.
    AtomicReplace {
        inputs: Vec<String>,
        outputs: Vec<TokenRecord>,
        record: ReplacementRecord,
        next: Box<dyn FnOnce(()) -> LedgerEffect<A> + Send>,
    },
    /// Fail with an error message. Short-circuits the effect chain.
    Fail { error: String },
}

impl<A: Send + 'static> LedgerEffect<A> {
    pub fn pure(a: A) -> Self {
        LedgerEffect::Pure(a)
    }

    pub fn fail(msg: impl Into<String>) -> Self {
        LedgerEffect::Fail { error: msg.into() }
    }

    /// Monadic bind: chain the output of this effect into a new effect.
    pub fn bind<B: Send + 'static>(
        self,
        f: impl FnOnce(A) -> LedgerEffect<B> + Send + 'static,
    ) -> LedgerEffect<B> {
        match self {
            LedgerEffect::Pure(a) => f(a),
            LedgerEffect::Fail { error } => LedgerEffect::Fail { error },
            LedgerEffect::GetToken { hash, next } => LedgerEffect::GetToken {
                hash,
                next: Box::new(move |tok| next(tok).bind(f)),
            },
            LedgerEffect::InsertToken { record, next } => LedgerEffect::InsertToken {
                record,
                next: Box::new(move |()| next(()).bind(f)),
            },
            LedgerEffect::MarkSpent { hash, next } => LedgerEffect::MarkSpent {
                hash,
                next: Box::new(move |ok| next(ok).bind(f)),
            },
            LedgerEffect::GetMiningState { next } => LedgerEffect::GetMiningState {
                next: Box::new(move |s| next(s).bind(f)),
            },
            LedgerEffect::UpdateMiningState { state, next } => LedgerEffect::UpdateMiningState {
                state,
                next: Box::new(move |()| next(()).bind(f)),
            },
            LedgerEffect::AtomicReplace {
                inputs,
                outputs,
                record,
                next,
            } => LedgerEffect::AtomicReplace {
                inputs,
                outputs,
                record,
                next: Box::new(move |()| next(()).bind(f)),
            },
        }
    }
}

// ── Smart constructors ──────────────────────────────────────────

pub fn get_token(hash: String) -> LedgerEffect<Option<TokenRecord>> {
    LedgerEffect::GetToken {
        hash,
        next: Box::new(LedgerEffect::Pure),
    }
}

pub fn insert_token(record: TokenRecord) -> LedgerEffect<()> {
    LedgerEffect::InsertToken {
        record,
        next: Box::new(LedgerEffect::Pure),
    }
}

pub fn mark_spent(hash: String) -> LedgerEffect<bool> {
    LedgerEffect::MarkSpent {
        hash,
        next: Box::new(LedgerEffect::Pure),
    }
}

pub fn get_mining_state() -> LedgerEffect<Option<MiningState>> {
    LedgerEffect::GetMiningState {
        next: Box::new(LedgerEffect::Pure),
    }
}

pub fn update_mining_state(state: MiningState) -> LedgerEffect<()> {
    LedgerEffect::UpdateMiningState {
        state,
        next: Box::new(LedgerEffect::Pure),
    }
}

pub fn atomic_replace(
    inputs: Vec<String>,
    outputs: Vec<TokenRecord>,
    record: ReplacementRecord,
) -> LedgerEffect<()> {
    LedgerEffect::AtomicReplace {
        inputs,
        outputs,
        record,
        next: Box::new(LedgerEffect::Pure),
    }
}
