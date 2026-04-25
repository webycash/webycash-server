//! Webcash flavor binary.
//!
//! Boots `Server<Webcash, RedisStore<Webcash, WebcashLegacyKeys>>` with the
//! new generic `server-core`. Wire format stays bit-for-bit compatible with
//! `https://webcash.org` production — `webycash-conformance` gates every
//! change.
//!
//! Endpoints currently wired:
//!   - `GET  /api/v1/target`
//!   - `POST /api/v1/health_check`
//!   - `GET  /terms`, `/terms/text`
//!
//! Remaining (replace, mining_report, burn, stats) land in M1 follow-ups.

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use webycash_asset_webcash::Webcash;
use webycash_mining::{MiningConfig, MiningMode};
use webycash_server_core::{serve, ServeConfig, Server};
use webycash_storage::redis_backend::RedisStore;
use webycash_storage::WebcashLegacyKeys;

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

    let mining = MiningConfig {
        mode: match mode.as_str() {
            "production" => MiningMode::webcash_production(),
            _ => MiningMode::webcash_testnet(),
        },
        ..MiningConfig::default()
    };

    let bind_addr = SocketAddr::from_str(&bind)
        .with_context(|| format!("WEBCASH_BIND_ADDR is not a valid socket address: {bind}"))?;
    let cfg = ServeConfig { bind_addr, mining };

    let store = RedisStore::<Webcash, _>::new(&redis_url, WebcashLegacyKeys)
        .await
        .with_context(|| format!("connecting to Redis at {redis_url}"))?;

    let server = Server::new(cfg, store);

    tracing::info!(
        asset = "webcash",
        %bind,
        %mode,
        %redis_url,
        "server-webcash booting"
    );
    serve(server).await
}
