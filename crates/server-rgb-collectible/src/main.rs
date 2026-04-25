//! RGB21 collectible (NFT) flavor binary.
//!
//! Boots `Server<RgbCollectible, S>` and dispatches via
//! `serve_collectible()` in server-core. Non-splittable: exposes
//! `/api/v1/transfer` (1:1 ownership move) instead of `/replace`, and
//! `/api/v1/burn_collectible` instead of `/burn`. Issuer-signed mint via
//! `/api/v1/issue` shares the same Ed25519 envelope as RGB20 / Voucher.

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use webycash_asset_rgb::RgbCollectible;
use webycash_auth::IssuerRegistry;
use webycash_mining::{MiningConfig, MiningMode};
use webycash_server_core::{serve_collectible, ServeConfig, Server};
#[cfg(feature = "fdb")]
use webycash_storage::fdb_backend::FdbStore;
#[cfg(feature = "fdb")]
use webycash_storage::redis_fdb_backend::RedisFdbStore;
use webycash_storage::dynamodb_backend::DynamoDbStore;
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
    let db_backend = std::env::var("WEBCASH_DB_BACKEND").unwrap_or_else(|_| "redis".to_string());

    let mut mining = MiningConfig {
        mode: match mode.as_str() {
            "production" => MiningMode::Disabled,
            _ => MiningMode::Disabled,
        },
        ..MiningConfig::default()
    };
    if let Ok(d) = std::env::var("WEBYCASH_DIFFICULTY") {
        if let Ok(bits) = d.parse::<u32>() {
            mining.mode = MiningMode::Fixed { difficulty: bits };
        }
    }

    let bind_addr = SocketAddr::from_str(&bind)
        .with_context(|| format!("WEBCASH_BIND_ADDR is not a valid socket address: {bind}"))?;
    let cfg = ServeConfig { bind_addr, mining };

    let mut issuers = IssuerRegistry::new();
    if let Ok(raw) = std::env::var("WEBYCASH_ISSUERS") {
        for entry in raw.split(',').filter(|s| !s.is_empty()) {
            let (fp, pk_hex) = entry
                .split_once(':')
                .context("WEBYCASH_ISSUERS entries must be `fp:hex_pubkey`")?;
            issuers
                .add_hex(fp, pk_hex)
                .with_context(|| format!("registering issuer {fp}"))?;
            tracing::info!(issuer = fp, "registered");
        }
    }

    tracing::info!(
        asset = "rgb-collectible",
        %bind,
        %mode,
        backend = %db_backend,
        "server-rgb-collectible booting"
    );

    macro_rules! finish {
        ($store:expr) => {{
            let server = Server::new(cfg, $store);
            let server = if issuers.is_empty() {
                tracing::warn!("no issuers configured; /api/v1/issue will reject");
                server
            } else {
                tracing::info!(count = issuers.len(), "issuer registry loaded");
                server.with_issuers(issuers)
            };
            serve_collectible(server).await
        }};
    }

    match db_backend.as_str() {
        "redis" => {
            let redis_url = std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let store = RedisStore::<RgbCollectible, _>::new(&redis_url, NamespacedKeys)
                .await
                .with_context(|| format!("connecting to Redis at {redis_url}"))?;
            finish!(store)
        }
        "dynamodb" => {
            let aws_config =
                aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let mut sdk_builder = aws_sdk_dynamodb::config::Builder::from(&aws_config);
            if let Ok(endpoint) = std::env::var("DYNAMODB_ENDPOINT") {
                sdk_builder = sdk_builder.endpoint_url(endpoint);
            }
            let client = aws_sdk_dynamodb::Client::from_conf(sdk_builder.build());
            let store = DynamoDbStore::<RgbCollectible, _>::new(client, NamespacedKeys);
            store.ensure_tables().await.context("ensure_tables on DynamoDB")?;
            finish!(store)
        }
        #[cfg(feature = "fdb")]
        "fdb" => {
            let network = unsafe { ::foundationdb::boot() };
            std::mem::forget(network);
            let cluster_file = std::env::var("FDB_CLUSTER_FILE").ok();
            let store = FdbStore::<RgbCollectible, _>::new(cluster_file.as_deref(), NamespacedKeys)
                .context("opening FoundationDB")?;
            finish!(store)
        }
        #[cfg(feature = "fdb")]
        "redis_fdb" => {
            let network = unsafe { ::foundationdb::boot() };
            std::mem::forget(network);
            let redis_url = std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let cluster_file = std::env::var("FDB_CLUSTER_FILE").ok();
            let store = RedisFdbStore::<RgbCollectible, _>::new(
                &redis_url,
                cluster_file.as_deref(),
                NamespacedKeys,
            )
            .await
            .context("opening Redis+FDB composite")?;
            finish!(store)
        }
        #[cfg(not(feature = "fdb"))]
        "fdb" | "redis_fdb" => {
            anyhow::bail!("WEBCASH_DB_BACKEND={db_backend} requires the `fdb` cargo feature")
        }
        other => anyhow::bail!("unknown WEBCASH_DB_BACKEND: {other}"),
    }
}
