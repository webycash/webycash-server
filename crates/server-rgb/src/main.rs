//! RGB flavor binary (RGB20-fungible / splittable).

use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use webycash_asset_rgb::RgbFungible;
use webycash_auth::IssuerRegistry;
use webycash_mining::{MiningConfig, MiningMode};
use webycash_server_core::{serve_issued, ServeConfig, Server};
use webycash_storage::dynamodb_backend::DynamoDbStore;
#[cfg(feature = "fdb")]
use webycash_storage::fdb_backend::FdbStore;
use webycash_storage::redis_backend::RedisStore;
#[cfg(feature = "fdb")]
use webycash_storage::redis_fdb_backend::RedisFdbStore;
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

    let mut issuers = IssuerRegistry::new();
    if let Ok(raw) = std::env::var("WEBYCASH_ISSUERS") {
        for entry in raw.split(',').filter(|s| !s.is_empty()) {
            let (fp, pk_hex) = entry
                .split_once(':')
                .context("WEBYCASH_ISSUERS entries must be `fp:hex_pubkey`")?;
            issuers
                .add_hex(fp, pk_hex)
                .with_context(|| format!("registering issuer {fp}"))?;
            tracing::info!(issuer = fp, "registered (raw ed25519)");
        }
    }
    if let Ok(path) = std::env::var("WEBYCASH_ISSUER_PGP_CERTS") {
        let blob = std::fs::read_to_string(&path)
            .with_context(|| format!("reading WEBYCASH_ISSUER_PGP_CERTS={path}"))?;
        for cert in split_pgp_blocks(&blob) {
            let fp = issuers
                .add_pgp_armored(&cert)
                .context("registering OpenPGP V4 cert")?;
            tracing::info!(issuer = %fp, "registered (pgp v4)");
        }
    }

    tracing::info!(
        asset = "rgb-fungible",
        %bind,
        %mode,
        backend = %db_backend,
        "server-rgb booting"
    );

    macro_rules! finish {
        ($store:expr) => {{
            let server = Server::new(cfg, $store);
            let server = if issuers.is_empty() {
                tracing::warn!("no issuers configured; /api/v1/issue will reject all requests");
                server
            } else {
                tracing::info!(count = issuers.len(), "issuer registry loaded");
                server.with_issuers(issuers)
            };
            serve_issued(server).await
        }};
    }

    match db_backend.as_str() {
        "redis" => {
            let redis_url =
                std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let store = RedisStore::<RgbFungible, _>::new(&redis_url, NamespacedKeys)
                .await
                .with_context(|| format!("connecting to Redis at {redis_url}"))?;
            finish!(store)
        }
        "dynamodb" => {
            let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let mut sdk_builder = aws_sdk_dynamodb::config::Builder::from(&aws_config);
            if let Ok(endpoint) = std::env::var("DYNAMODB_ENDPOINT") {
                sdk_builder = sdk_builder.endpoint_url(endpoint);
            }
            let client = aws_sdk_dynamodb::Client::from_conf(sdk_builder.build());
            let store = DynamoDbStore::<RgbFungible, _>::new(client, NamespacedKeys);
            store
                .ensure_tables()
                .await
                .context("ensure_tables on DynamoDB")?;
            finish!(store)
        }
        #[cfg(feature = "fdb")]
        "fdb" => {
            let network = unsafe { ::foundationdb::boot() };
            std::mem::forget(network);
            let cluster_file = std::env::var("FDB_CLUSTER_FILE").ok();
            let store = FdbStore::<RgbFungible, _>::new(cluster_file.as_deref(), NamespacedKeys)
                .context("opening FoundationDB")?;
            finish!(store)
        }
        #[cfg(feature = "fdb")]
        "redis_fdb" => {
            let network = unsafe { ::foundationdb::boot() };
            std::mem::forget(network);
            let redis_url =
                std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let cluster_file = std::env::var("FDB_CLUSTER_FILE").ok();
            let store = RedisFdbStore::<RgbFungible, _>::new(
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

/// Split a multi-cert ASCII-armored blob into individual OpenPGP V4 blocks.
fn split_pgp_blocks(blob: &str) -> Vec<String> {
    const BEGIN: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
    const END: &str = "-----END PGP PUBLIC KEY BLOCK-----";
    let mut out = Vec::new();
    let mut rest = blob;
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start..];
        if let Some(end) = after.find(END) {
            let block_end = end + END.len();
            out.push(after[..block_end].to_string());
            rest = &after[block_end..];
        } else {
            break;
        }
    }
    out
}
