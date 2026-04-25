//! Issuer authentication — OpenPGP V4 Ed25519 fingerprints, nonce caches,
//! signature verification, signing helpers.
//!
//! Used by RGB and Voucher server flavors for `/issue` endpoints. Webcash
//! flavor does not depend on this crate.
//!
//! Implementation lands in M3.
