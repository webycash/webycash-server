//! AluVM execution context — load contract, run transition, return diagnostics.
//!
//! Single integration point used by `asset-rgb` (server) and (via the same
//! crate compiled to WASM) by webylib's `wallet-rgb`. Tracks
//! https://docs.aluvm.org and https://www.contractum.org.
//!
//! Lands in M3.
