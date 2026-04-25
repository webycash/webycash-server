//! Mining configuration and difficulty algorithms.
//!
//! Three modes per flavor (operator-configurable via env):
//!   - `Disabled` — no mining endpoint exposed
//!   - `Fixed { difficulty }` — constant target
//!   - `Dynamic { initial, target_secs, reports_per_epoch }` — self-adjusting
//!
//! Defaults: Webcash unchanged (Dynamic prod, Fixed testnet); RGB and Voucher
//! default to Dynamic, infinite issuance cap. See plan §"Mining Configuration".
//!
//! Implementation lands in M1 (Webcash baseline) then generalises in M3.
