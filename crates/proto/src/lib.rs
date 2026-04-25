//! Wire-format types, parsers, and JSON shapes for the asset-gated webycash protocol.
//!
//! This crate provides the **shared parser building blocks** used by all
//! three asset flavors (`asset-webcash`, `asset-rgb`, `asset-voucher`):
//! amount parsers, hex parsers, common JSON envelopes (legalese, mining
//! report shapes), and shared error types.
//!
//! The asset-specific token grammars (`e{amt}:secret:{hex}`,
//! `e{amt}:secret:{hex}:{contract}:{issuer}`, etc.) live in their respective
//! `asset-*` crates and reuse the parsers exposed here.

#![forbid(unsafe_code)]

pub mod parsers;

pub use parsers::{amount_parser, hex64};
