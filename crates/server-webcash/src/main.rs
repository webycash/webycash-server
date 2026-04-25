//! Webcash flavor binary.
//!
//! Boots `Server<Webcash>` with the new generic `server-core`. Wire format
//! stays bit-for-bit compatible with `https://webcash.org` production —
//! `webycash-conformance` gates every change.
//!
//! Currently serves:
//!   - `GET  /api/v1/target`
//!   - `GET  /terms` / `/terms/text`
//!
//! Remaining endpoints (replace / health_check / mining_report / burn /
//! stats) migrate from `crates/server/` in follow-up commits within M1.

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use webycash_asset_webcash::Webcash;
use webycash_mining::{MiningConfig, MiningMode};
use webycash_server_core::{serve, ServeConfig, Server};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("webycash_server_core=info,info")),
        )
        .init();

    let bind = std::env::var("WEBCASH_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let mode = std::env::var("WEBCASH_MODE").unwrap_or_else(|_| "testnet".to_string());

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
    let server: Server<Webcash> = Server::new(cfg);

    tracing::info!(asset = "webcash", %bind, %mode, "server-webcash booting");
    serve(server).await
}
