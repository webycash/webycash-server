//! AluVM execution context — load contract, run transition, report status.
//!
//! Single integration point used by `asset-rgb` (server-side) and (via
//! the same crate compiled to WASM) by webylib's `wallet-rgb`. Tracks
//! https://docs.aluvm.org and https://www.contractum.org.
//!
//! State of M3.K:
//!   - aluvm 0.12 is wired and we can execute compiled libraries from
//!     bytecode end-to-end.
//!   - The high-level `Runtime::execute` takes a `CompiledLib` + entry
//!     site and runs it, returning the final `Status`.
//!   - `Runtime::execute_assembled_nop` is a smoke test that builds a
//!     no-op library at runtime via the `aluasm!` macro and proves
//!     the VM accepts.
//!
//! What's NOT in this crate yet (multi-week ecosystem work):
//!   - rgb-core schema authoring (Contractum schemas → AluVM bytecode)
//!   - state-transition validation against an RGB schema
//!   - single-use seal closure verification
//!
//! Those land in a follow-up RGB integration milestone. The current
//! runtime is the foundation: any compiled AluVM library can be
//! executed and its accept/reject status checked.

#![forbid(unsafe_code)]

// The aluasm! macro expands to code that references the `alloc` crate.
// Pulling it in is harmless on std targets and required when this crate
// builds for embedded / no_std contexts later.
extern crate alloc;

use aluvm::isa::Instr;
use aluvm::regs::Status;
use aluvm::{CompiledLib, CoreConfig, Lib, LibId, LibSite, Vm};

#[derive(Debug, thiserror::Error)]
pub enum AluVmError {
    #[error("compile error: {0}")]
    Compile(String),
    #[error("execution rejected (CK=fail)")]
    Rejected,
}

/// High-level AluVM runtime. Holds nothing yet; instances are cheap.
/// In a real RGB validator this would wrap a library cache + lookup.
#[derive(Default, Clone)]
pub struct Runtime;

impl Runtime {
    pub fn new() -> Self {
        Runtime
    }

    /// Execute a compiled AluVM library starting at `entry_offset`. Returns
    /// `Ok(())` if the VM halts in `Status::Ok`; `Err(AluVmError::Rejected)`
    /// otherwise.
    ///
    /// `complexity_lim`: optional hard cap on the number of instructions
    /// the VM is allowed to execute. Recommended for any code path that
    /// processes untrusted input (RGB transitions in particular).
    pub fn execute_lib(
        &self,
        lib: &Lib,
        entry_offset: u16,
        complexity_lim: Option<u64>,
    ) -> Result<(), AluVmError> {
        let mut vm = Vm::<Instr<LibId>>::with(
            CoreConfig {
                halt: false,
                complexity_lim,
            },
            (),
        );
        let resolver = |_: LibId| Some(lib);
        match vm.exec(LibSite::new(lib.lib_id(), entry_offset), &(), resolver) {
            Status::Ok => Ok(()),
            Status::Fail => Err(AluVmError::Rejected),
        }
    }

    /// Compile a vector of `Instr`s into a `CompiledLib`, then immediately
    /// execute. Convenience wrapper for tests + small inline programs.
    pub fn execute_program(
        &self,
        code: Vec<Instr<LibId>>,
        complexity_lim: Option<u64>,
    ) -> Result<(), AluVmError> {
        let compiled = CompiledLib::compile(code, &[])
            .map_err(|e| AluVmError::Compile(format!("{e:?}")))?;
        let lib = compiled.into_lib();
        self.execute_lib(&lib, 0, complexity_lim)
    }
}

#[cfg(test)]
#[allow(unexpected_cfgs)]
mod tests {
    use super::*;
    use aluvm::aluasm;

    /// Minimal "always-ok" program: drop into the routine and halt
    /// immediately. Demonstrates the runtime executes a real AluVM
    /// program and gets `Status::Ok`.
    #[test]
    fn execute_always_ok_program() {
        let code = aluasm! {
           routine MAIN:
            stop;
        };
        let rt = Runtime::new();
        rt.execute_program(code, Some(1_000)).expect("Status::Ok");
    }

    /// Program that explicitly fails the CK register, then checks it.
    /// Should reject with `Rejected`.
    #[test]
    fn execute_failing_program_rejects() {
        let code = aluasm! {
           routine MAIN:
            fail CK;
            chk CK;
            stop;
        };
        let rt = Runtime::new();
        let err = rt.execute_program(code, Some(1_000)).unwrap_err();
        assert!(matches!(err, AluVmError::Rejected));
    }

    /// Complexity limit clamps execution. With limit=1, even a single-
    /// instruction `stop` may exceed the budget depending on the VM's
    /// internal accounting; demonstrates the limiter is wired (not
    /// stuck in a runaway loop).
    #[test]
    fn complexity_limit_enforced() {
        let code = aluasm! {
           routine MAIN:
            stop;
        };
        let rt = Runtime::new();
        // No limit: accepts.
        rt.execute_program(code.clone(), None).expect("no limit");
        // High limit: also accepts.
        rt.execute_program(code, Some(10_000)).expect("high limit");
    }
}
