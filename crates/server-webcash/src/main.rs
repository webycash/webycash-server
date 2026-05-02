//! Webcash flavor binary.
//!
//! Boots `Server<Webcash, S>` where `S` is selected at runtime from
//! `WEBCASH_DB_BACKEND`:
//!   - `redis` (default): RedisStore<Webcash, WebcashLegacyKeys>
//!   - `dynamodb`: DynamoDbStore<Webcash, WebcashLegacyKeys>
//!
//! Wire format stays bit-for-bit compatible with `https://webcash.org`
//! production — `webycash-conformance` gates every change.

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use webycash_asset_webcash::Webcash;
use webycash_mining::{MiningConfig, MiningMode};
use webycash_server_core::{serve, ServeConfig, Server};
#[cfg(feature = "fdb")]
use webycash_storage::fdb_backend::FdbStore;
#[cfg(feature = "fdb")]
use webycash_storage::redis_fdb_backend::RedisFdbStore;
use webycash_storage::dynamodb_backend::DynamoDbStore;
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
    let db_backend = std::env::var("WEBCASH_DB_BACKEND").unwrap_or_else(|_| "redis".to_string());

    let mut mining = MiningConfig {
        mode: match mode.as_str() {
            "production" => MiningMode::webcash_production(),
            _ => MiningMode::webcash_testnet(),
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

    tracing::info!(
        asset = "webcash",
        %bind,
        %mode,
        backend = %db_backend,
        "server-webcash booting"
    );

    match db_backend.as_str() {
        "redis" => {
            let redis_url = std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let store = RedisStore::<Webcash, _>::new(&redis_url, WebcashLegacyKeys)
                .await
                .with_context(|| format!("connecting to Redis at {redis_url}"))?;
            serve(Server::new(cfg, store)).await
        }
        "dynamodb" => {
            let aws_config =
                aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let mut sdk_builder = aws_sdk_dynamodb::config::Builder::from(&aws_config);
            if let Ok(endpoint) = std::env::var("DYNAMODB_ENDPOINT") {
                sdk_builder = sdk_builder.endpoint_url(endpoint);
            }
            let client = aws_sdk_dynamodb::Client::from_conf(sdk_builder.build());
            let store = DynamoDbStore::<Webcash, _>::new(client, WebcashLegacyKeys);
            store
                .ensure_tables()
                .await
                .context("ensure_tables on DynamoDB")?;
            serve(Server::new(cfg, store)).await
        }
        #[cfg(feature = "fdb")]
        "fdb" => {
            // Caller is responsible for foundationdb::boot() before this point.
            let network = unsafe { ::foundationdb::boot() };
            std::mem::forget(network);
            let cluster_file = std::env::var("FDB_CLUSTER_FILE").ok();
            let store = FdbStore::<Webcash, _>::new(cluster_file.as_deref(), WebcashLegacyKeys)
                .context("opening FoundationDB")?;
            serve(Server::new(cfg, store)).await
        }
        #[cfg(feature = "fdb")]
        "redis_fdb" => {
            let network = unsafe { ::foundationdb::boot() };
            std::mem::forget(network);
            let redis_url = std::env::var("REDIS_URL")
                .unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let cluster_file = std::env::var("FDB_CLUSTER_FILE").ok();
            let store = RedisFdbStore::<Webcash, _>::new(
                &redis_url,
                cluster_file.as_deref(),
                WebcashLegacyKeys,
            )
            .await
            .context("opening Redis+FDB composite")?;
            serve(Server::new(cfg, store)).await
        }
        #[cfg(not(feature = "fdb"))]
        "fdb" | "redis_fdb" => anyhow::bail!(
            "WEBCASH_DB_BACKEND={db_backend} requires the `fdb` cargo feature; \
             rebuild with `cargo build --features fdb` (FoundationDB C client must be installed)"
        ),
        other => anyhow::bail!(
            "unknown WEBCASH_DB_BACKEND: {other} (must be `redis`, `dynamodb`, `fdb`, or `redis_fdb`)"
        ),
    }
}
