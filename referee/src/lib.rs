//! Referee — Webcash ↔ Bitcoin ARK swap helper.
//!
//! Implements the protocol specified in
//! `webycash-server/docs/referee-zkp-based-swap.md` §4. This crate is **not
//! part of the default cargo build**; the deployer opts in via
//! `cargo build -p referee`.
//!
//! ## What this crate is, in one paragraph
//!
//! A Rust service that mediates a one-of-a-kind cross-rail flow: Bob holds
//! webcash; Alice holds a Bitcoin ARK vtxo. Webcash transfers a *secret*;
//! ARK transfers a *signature*. Neither rail has a primitive the other
//! understands. The referee glues them together by (a) verifying both
//! parties' encrypted payloads via Groth16 ZKPs against the public hashes
//! committed in the audit chain, (b) calling the webcash server to confirm
//! Bob's leg moved, and (c) co-signing the 2-of-2 MuSig2 vtxo so Bob can
//! claim it. The referee is **non-custodial**: every encrypted payload is
//! addressed to the counterparty's PGP pubkey, so the referee receives and
//! forwards ciphertext only. Alice's `TX_refund` partial-sig is never
//! submitted to the referee at all. The only secret the referee owns is
//! its own Ed25519 identity key (for signed audit log + webhook auth) and
//! its own MuSig2 key-share. See `docs/trust-model.md`.
//!
//! ## Module map
//!
//! | Module | Purpose |
//! |---|---|
//! | [`config`] | Boot-time configuration (env + file). |
//! | [`error`] | Crate-wide error type. |
//! | [`sign`] | Ed25519 referee identity + canonical-message signing. |
//! | [`state`] | Phase-typed `SwapState<Phase>` typestate. Pure transitions. |
//! | [`zkp`] | Groth16 verifier interface (Bob's payload + Alice's signature circuits). |
//! | [`musig2`] | MuSig2 nonce/partial-sig handling for the referee's own key share. |
//! | [`push`] | External push-webhook caller. |
//! | [`store`] | Persistent swap-state store (in-memory default; Redis / DynamoDB / FoundationDB opt-in). |
//! | [`audit`] | Append-only signed audit log. |
//! | [`clients`] | Asset-rail HTTP clients (Webcash, RGB). |
//! | [`transaction`] | Top-level transaction model + projection. |
//! | [`api`] | Axum HTTP API surface. |
//!
//! ## Wallet implementor side is out of scope here
//!
//! Everything user-side — PGP encryption, ZKP *proving*, MuSig2 partial-sig
//! generation by Alice, Bitcoin ARK transaction construction, the
//! `insert_hook` / `invalidate_hook` callbacks — lives in the **extro-node**
//! project under `/Users/george/workspace/extro/`. This crate **only**
//! implements the server side. The contracts the wallet implementor must
//! satisfy are documented in `docs/hook-contract.md` and
//! `docs/musig2-ceremony.md`.
//!
//! ## Cryptographic invariants (enforced at the type level)
//!
//! - Alice's `TX_settle` MuSig2 partial-sig is only ever carried inside a
//!   `PgpEncrypted<AlicePartialSig>` newtype — the referee API only accepts
//!   it as ciphertext addressed to Bob's PGP pubkey.
//! - Alice's `TX_refund` MuSig2 partial-sig is **never** transmitted to
//!   the referee; it is local to her wallet.
//! - Bob's webcash secret `S_B` is only ever held inside a
//!   `PgpEncrypted<WebcashSecret>` newtype.
//! - The ZKP verifier consumes `(public_inputs, proof)`, never witnesses.
//!
//! See `docs/architecture.md` for the typestate diagram and the formal
//! statement of each invariant.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod api;
pub mod audit;
pub mod clients;
pub mod config;
pub mod error;
pub mod musig2;
pub mod push;
pub mod sign;
pub mod state;
pub mod store;
pub mod transaction;
pub mod zkp;

/// Build identifier returned from the future `/v1/version` endpoint.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use config::Config;
pub use error::{RefereeError, Result};
