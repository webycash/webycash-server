// webycash-server: all sub-crate source trees inlined as modules.
// Dependency order: each module only references modules declared above it.

#[path = "../crates/proto/src/lib.rs"]
pub mod proto;

#[path = "../crates/asset-core/src/lib.rs"]
pub mod asset_core;

#[path = "../crates/auth/src/lib.rs"]
pub mod auth;

#[path = "../crates/storage/src/lib.rs"]
pub mod storage;

#[path = "../crates/compute/src/lib.rs"]
pub mod compute;

#[path = "../crates/mining/src/lib.rs"]
pub mod mining;

#[path = "../crates/asset-webcash/src/lib.rs"]
pub mod asset_webcash;

#[cfg(feature = "rgb")]
#[path = "../crates/asset-rgb/src/lib.rs"]
pub mod asset_rgb;

#[cfg(feature = "voucher")]
#[path = "../crates/asset-voucher/src/lib.rs"]
pub mod asset_voucher;

#[path = "../crates/server-core/src/lib.rs"]
pub mod server_core;
