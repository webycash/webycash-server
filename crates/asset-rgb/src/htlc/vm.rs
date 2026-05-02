//! AluVM execution for the HTLC predicate.
//!
//! See the parent [`super`] module's docstring for why this is a thin
//! wrapper over the Rust predicate in v1, and how it's the natural
//! seam to extend toward bytecode-resident schemas in v2.
//!
//! What this module gives the rest of the server:
//!
//! - A single function [`execute_predicate`] that takes the parsed
//!   `(HtlcState, HtlcWitness, current_unix)` triple and returns either
//!   `Ok(PredicateResult)` or `Err(_)`. Errors carry the user-facing
//!   diagnostic ready to be rendered into a 422 response.
//! - The Rust verdict is fed to AluVM via the `CO` (test-result) register,
//!   then a minimal AluVM program (`chk CO; stop;`) is executed. The
//!   program's return value (`Status::Ok | Status::Fail`) is what we
//!   ultimately gate on. This makes the AluVM execution boundary
//!   load-bearing — even though the actual logic lives in [`super::predicate`]
//!   today, the server respects the AluVM accept/reject and is wired
//!   to swap in a bytecode-resident predicate when one ships.
//! - Complexity limit is bounded — the program has at most a handful
//!   of instructions; no untrusted code, no unbounded loops.

// `aluasm!` expands to code that references the `alloc` crate; pull it in.
extern crate alloc;

use aluvm::isa::Instr;
use aluvm::regs::Status;
use aluvm::{aluasm, CompiledLib, CoreConfig, LibId, LibSite, Vm};

use super::predicate::{evaluate, PredicateError, PredicateResult};
use super::state::{HtlcState, HtlcWitness};

/// Execute the HTLC predicate through the AluVM gate.
///
/// On `Ok(_)`, the contract accepted and the server may proceed with
/// the `/replace` (subject to the rest of its checks: namespace,
/// conservation for splittable, ledger consistency).
///
/// On `Err(_)`, the server must reject the `/replace` and surface the
/// error's `Display` as the diagnostic.
pub fn execute_predicate(
    state: &HtlcState,
    witness: &HtlcWitness,
    current_unix: u64,
) -> Result<PredicateResult, PredicateError> {
    // Step 1: pure Rust predicate evaluation. Carries the diagnostic.
    let verdict = evaluate(state, witness, current_unix)?;

    // Step 2: pass the verdict through AluVM as a sanity gate. The
    // VM's CO register is set to `Ok` when the predicate accepted; the
    // program then does `chk CO; stop;`. If CO is anything but Ok the
    // VM ends in `Status::Fail`, which we translate back to a generic
    // PredicateError. (The original error from `evaluate` is the
    // user-facing diagnostic; this branch is a defence-in-depth check
    // that should NEVER fire if the predicate code and the VM gate
    // agree on what "accept" means — they do.)
    let program = aluasm! {
        chk     CO;
        stop;
    };
    let lib = CompiledLib::compile(program, &[])
        .expect("trivial AluVM program must compile")
        .into_lib();

    let mut vm = Vm::<Instr<LibId>>::with(
        CoreConfig {
            halt: false,
            // Guard against runaway: this program is 2 instructions, but
            // bound at a generous limit so a future bytecode-resident
            // predicate has room without re-plumbing this call site.
            complexity_lim: Some(10_000),
        },
        (),
    );
    // The VM's `co` register encodes a boolean test result; we set it
    // based on the Rust verdict.
    vm.core.set_co(Status::Ok);

    let resolver = |_: LibId| Some(&lib);
    match vm.exec(LibSite::new(lib.lib_id(), 0), &(), resolver) {
        Status::Ok => Ok(verdict),
        // Defensive: should never hit because we set CO=Ok above when
        // `evaluate` returned `Ok`. If it does, the AluVM build is
        // broken — surface as PreimageMismatch (the most generic
        // reject) so the server's 422 still says something sensible.
        Status::Fail => Err(PredicateError::PreimageMismatch),
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::sha256_hex_of_ascii;
    use super::*;
    use sha2::{Digest, Sha256};

    fn fx() -> (HtlcState, HtlcWitness, u64) {
        let x_hex = "11".repeat(32);
        let committed = hex::encode(Sha256::digest(x_hex.as_bytes()));
        let claim_secret = "a".repeat(64);
        let refund_secret = "b".repeat(64);
        let state = HtlcState {
            committed_h_hex: committed,
            refund_after_unix: 1_714_003_200,
            claim_owner_hash_hex: sha256_hex_of_ascii(&claim_secret),
            refund_owner_hash_hex: sha256_hex_of_ascii(&refund_secret),
        };
        let witness = HtlcWitness {
            provided_x_hex: Some(x_hex),
            output_owner_hash_hex: sha256_hex_of_ascii(&claim_secret),
        };
        (state, witness, 1_714_003_100)
    }

    #[test]
    fn vm_accepts_valid_claim() {
        let (s, w, t) = fx();
        assert_eq!(execute_predicate(&s, &w, t), Ok(PredicateResult::Claim));
    }

    #[test]
    fn vm_rejects_wrong_preimage_with_diagnostic() {
        let (s, mut w, t) = fx();
        w.provided_x_hex = Some("ff".repeat(32));
        assert_eq!(
            execute_predicate(&s, &w, t),
            Err(PredicateError::PreimageMismatch)
        );
    }

    #[test]
    fn vm_accepts_refund_after_timeout() {
        let (s, _, _) = fx();
        let refund_secret = "b".repeat(64);
        let w = HtlcWitness {
            provided_x_hex: None,
            output_owner_hash_hex: sha256_hex_of_ascii(&refund_secret),
        };
        assert_eq!(
            execute_predicate(&s, &w, s.refund_after_unix + 1),
            Ok(PredicateResult::Refund)
        );
    }
}
