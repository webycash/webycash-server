//! Voucher flavor binary.
//!
//! Boots `Server<Voucher, RedisStore<Voucher, NamespacedKeys>>`. Vouchers
//! are always-splittable bearer credits, issuer-namespaced by
//! `(contract_id, issuer_fp)`. Endpoints match the RGB fungible binary.

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use webycash_asset_voucher::Voucher;
use webycash_mining::{MiningConfig, MiningMode};
use webycash_server_core::{serve, ServeConfig, Server};
use webycash_storage::redis_backend::RedisStore;
use webycash_storage::NamespacedKeys;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,webycash_server_core=info")),
        )
        .init();

    let bind = std::env::var("WEBCASH_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let mode = std::env::var("WEBCASH_MODE").unwrap_or_else(|_| "testnet".to_string());
    let redis_url =
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());

    let mut mining = MiningConfig {
        mode: match mode.as_str() {
            "production" => MiningMode::issued_default(),
            _ => MiningMode::Fixed { difficulty: 4 },
        },
        ..MiningConfig::default()
    };
    if let Ok(d) = std::env::var("WEBYCASH_DIFFICULTY") {
        if let Ok(bits) = d.parse::<u32>() {
            mining.mode = MiningMode::Fixed { difficulty: bits };
        }
    }
    if std::env::var("WEBYCASH_MINING_MODE").as_deref() == Ok("disabled") {
        mining.mode = MiningMode::Disabled;
    }

    let bind_addr = SocketAddr::from_str(&bind)
        .with_context(|| format!("WEBCASH_BIND_ADDR is not a valid socket address: {bind}"))?;
    let cfg = ServeConfig { bind_addr, mining };

    let store = RedisStore::<Voucher, _>::new(&redis_url, NamespacedKeys)
        .await
        .with_context(|| format!("connecting to Redis at {redis_url}"))?;

    let server = Server::new(cfg, store);

    tracing::info!(
        asset = "voucher",
        %bind,
        %mode,
        %redis_url,
        "server-voucher booting"
    );
    serve(server).await
}
