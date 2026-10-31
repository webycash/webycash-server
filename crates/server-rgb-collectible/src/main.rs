//! RGB21 collectible flavor binary (non-splittable, licensable —
//! Perpetual or Royalties License).
//!
//! Boots `Server<RgbCollectible, S>` and dispatches via
//! `serve_collectible()` in server-core. Same endpoint surface as the
//! splittable flavors — `/api/v1/replace` for writes, `/api/v1/burn`
//! for destruction — but the handler enforces 1:1 arity (single input,
//! single output, same namespace) instead of an N:M conservation law.
//!
//! `/api/v1/mining_report` is statically absent: collectibles can't be
//! PoW-mined; issuance is operator-signed only via `/api/v1/issue`
//! (Ed25519 envelope shared with RGB20 / Voucher).

use anyhow::Context;
use tracing_subscriber::EnvFilter;
use webycash_server::asset_rgb::RgbCollectible;
use webycash_server::auth::IssuerRegistry;
use webycash_server::server_core::{serve_collectible, MiningDefaults, ServeConfig, Server};
use webycash_server::storage::dynamodb_backend::DynamoDbStore;
#[cfg(feature = "fdb")]
use webycash_server::storage::fdb_backend::FdbStore;
use webycash_server::storage::redis_backend::RedisStore;
#[cfg(feature = "fdb")]
use webycash_server::storage::redis_fdb_backend::RedisFdbStore;
use webycash_server::storage::worker_proxy_backend::WorkerProxyStore;
use webycash_server::storage::NamespacedKeys;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,webycash_server_core=info")),
        )
        .init();

    let (mode, cfg) =
        ServeConfig::from_vars(MiningDefaults::collectible(), |k| std::env::var(k).ok())?;
    let bind = cfg.bind_addr;
    let db_backend = std::env::var("WEBCASH_DB_BACKEND").unwrap_or_else(|_| "redis".to_string());

    let mut issuers = IssuerRegistry::new();
    if let Ok(raw) = std::env::var("WEBYCASH_ISSUERS") {
        for fp in issuers
            .add_hex_list(&raw)
            .context("parsing WEBYCASH_ISSUERS")?
        {
            tracing::info!(issuer = %fp, "registered (raw ed25519)");
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
        asset = "rgb-collectible",
        %bind,
        ?mode,
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
            let redis_url =
                std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
            let store = RedisStore::<RgbCollectible, _>::new(&redis_url, NamespacedKeys)
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
            let store = DynamoDbStore::<RgbCollectible, _>::new(client, NamespacedKeys);
            store
                .ensure_tables()
                .await
                .context("ensure_tables on DynamoDB")?;
            finish!(store)
        }
        "worker_proxy" => {
            let store = WorkerProxyStore::<RgbCollectible, _>::from_env(NamespacedKeys)?;
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
            let redis_url =
                std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".to_string());
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
