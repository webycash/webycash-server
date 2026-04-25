//! Generic asset-server core.
//!
//! Exposes `Server<A: Asset + MintableAsset>` parameterized over an asset
//! flavor. Compile-time trait bounds gate which endpoints exist:
//!   - `A: SplittableAsset` enables `/api/v1/replace`
//!   - `A: TransferableAsset` enables `/api/v1/transfer`
//!   - `A: IssuedAsset + MintableAsset` enables `/api/v1/issue` and `/issue_with_proof`
//!   - `A: MintableAsset` always enables `/api/v1/health_check`, `/mining_report`, `/burn`
//!
//! The three flavor binaries (`server-webcash`, `server-rgb`, `server-voucher`)
//! each instantiate `Server<A>` with their concrete asset type and bind to a
//! port. Implementation migrates from `crates/server/src/api/` in M1.
