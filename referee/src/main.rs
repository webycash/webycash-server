//! Referee binary entry point.
//!
//! Wires up the [`Orchestrator`] with the appropriate collaborators and
//! starts the Axum HTTP server. Configuration is loaded from environment
//! variables; see [`Config::from_env`] and `docs/deployment.md`.
//!
//! ## Build flavours
//!
//! Production cryptographic backends (real Groth16 verifier, real
//! MuSig2 signer) are gated behind cargo features. Storage backends
//! (Redis, DynamoDB, FoundationDB) match the asset-server matrix and
//! are also gated:
//!
//! | Build flags | Behaviour |
//! |---|---|
//! | (none) | **Dev-only**: mock crypto, in-memory store/audit. Refuses to start unless `REFEREE_ALLOW_MOCK_CRYPTO=1`. |
//! | `--features zkp-arkworks,musig2-real` | Real crypto. Add `--features dynamodb` (or `redis`, `fdb`) for persistent state. |
//! | Production | `--features zkp-arkworks,musig2-real,dynamodb` (or whichever store the deployment uses). |
//!
//! See `docs/deployment.md` for the full production checklist.

use std::sync::Arc;

use referee::api::orchestrator::Orchestrator;
use referee::api::router::build_router;
use referee::audit::{AuditLog, InMemoryAuditLog};
use referee::clients::{MockRgb, MockWebcash};
use referee::config::Config;
use referee::push::HttpPush;
use referee::sign::Identity;
use referee::store::{InMemoryStore, SwapStore};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "config error");
            tracing::info!(
                "Required env vars: REFEREE_BIND, REFEREE_IDENTITY_KEY_PATH, \
                 REFEREE_RGB_SERVER_URL, REFEREE_WEBCASH_SERVER_URL, \
                 REFEREE_PUSH_WEBHOOK_URL, REFEREE_PUSH_WEBHOOK_HMAC_KEY_PATH. \
                 Optional: WEBCASH_DB_BACKEND (redis|dynamodb|fdb|inmem), \
                 REDIS_URL, DYNAMODB_ENDPOINT, FDB_CLUSTER_FILE. \
                 See docs/deployment.md."
            );
            std::process::exit(2);
        }
    };

    refuse_mocks_in_production()?;

    let identity = Identity::load_from_file(&config.identity_key_path)?;
    let hmac_key_hex = std::fs::read_to_string(&config.push_webhook_hmac_key_path)
        .map_err(|e| anyhow::anyhow!("read push hmac key: {e}"))?;
    let hmac_key_bytes = hex::decode(hmac_key_hex.trim())
        .map_err(|e| anyhow::anyhow!("decode push hmac key: {e}"))?;

    let verifier = build_verifier();
    let musig = build_musig_signer();
    let (store, audit) = build_storage(&config).await?;

    let orch = Orchestrator {
        identity: Arc::new(identity),
        verifier,
        musig,
        webcash: Arc::new(MockWebcash::always_unspent()),
        rgb: Arc::new(MockRgb::new()),
        push: Arc::new(HttpPush::new(
            config.push_webhook_url.clone(),
            hmac_key_bytes,
        )),
        audit,
        store,
        swap_max_age_secs: config.swap_max_age_secs,
        insert_push_retry: config.insert_push_retry,
        retry_backoff: std::time::Duration::from_millis(config.retry_backoff_ms),
        callback_base_url: format!("http://{}/v1/swap", config.bind),
    };

    let app = build_router(Arc::new(orch));
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(addr = %config.bind, backend = %config.db_backend, "referee listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Refuse to boot if mock cryptographic backends would silently be wired
/// up. Operators must explicitly opt in via `REFEREE_ALLOW_MOCK_CRYPTO=1`
/// for dev/testing — production builds enable `zkp-arkworks` AND
/// `musig2-real` so this check passes automatically.
fn refuse_mocks_in_production() -> anyhow::Result<()> {
    let zkp_real = cfg!(feature = "zkp-arkworks");
    let musig_real = cfg!(feature = "musig2-real");
    if zkp_real && musig_real {
        return Ok(());
    }
    if std::env::var("REFEREE_ALLOW_MOCK_CRYPTO").as_deref() == Ok("1") {
        tracing::warn!(
            zkp_real,
            musig_real,
            "REFEREE_ALLOW_MOCK_CRYPTO=1 set — booting with mock cryptographic \
             backends. NEVER set this in production."
        );
        return Ok(());
    }
    anyhow::bail!(
        "refusing to boot with mock cryptographic backends. Build with \
         `cargo build --release -p referee --features zkp-arkworks,musig2-real` \
         for production, or set REFEREE_ALLOW_MOCK_CRYPTO=1 for dev/testing. \
         (zkp-arkworks={zkp_real}, musig2-real={musig_real})"
    );
}

/// Pick the swap store + audit log based on `WEBCASH_DB_BACKEND`.
/// Returns trait objects so the orchestrator stays storage-agnostic.
async fn build_storage(config: &Config) -> anyhow::Result<(Arc<dyn SwapStore>, Arc<dyn AuditLog>)> {
    match config.db_backend.as_str() {
        "inmem" => Ok((
            Arc::new(InMemoryStore::default()),
            Arc::new(InMemoryAuditLog::default()),
        )),
        #[cfg(feature = "redis")]
        "redis" => {
            let url = config
                .redis_url
                .clone()
                .unwrap_or_else(|| "redis://127.0.0.1:6379".to_string());
            let store = referee::store::redis::RedisSwapStore::new(&url).await?;
            let audit = referee::audit::redis::RedisAuditLog::new(&url).await?;
            Ok((Arc::new(store), Arc::new(audit)))
        }
        #[cfg(feature = "dynamodb")]
        "dynamodb" => {
            let aws_cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
            let mut sdk_builder = aws_sdk_dynamodb::config::Builder::from(&aws_cfg);
            if let Some(endpoint) = &config.dynamodb_endpoint {
                sdk_builder = sdk_builder.endpoint_url(endpoint);
            }
            let client = aws_sdk_dynamodb::Client::from_conf(sdk_builder.build());
            let store = referee::store::dynamodb::DynamoDbSwapStore::new(client.clone());
            store.ensure_tables().await?;
            let audit = referee::audit::dynamodb::DynamoDbAuditLog::new(client);
            audit.ensure_tables().await?;
            Ok((Arc::new(store), Arc::new(audit)))
        }
        #[cfg(feature = "fdb")]
        "fdb" => {
            let network = unsafe { ::foundationdb::boot() };
            std::mem::forget(network);
            let cluster = config.fdb_cluster_file.as_deref();
            let store = referee::store::fdb::FdbSwapStore::new(cluster)?;
            let audit = referee::audit::fdb::FdbAuditLog::new(cluster)?;
            Ok((Arc::new(store), Arc::new(audit)))
        }
        other => anyhow::bail!(
            "unknown WEBCASH_DB_BACKEND={other}. Build with the matching \
             feature flag (--features redis|dynamodb|fdb) and re-set the \
             env var. Default is `inmem` (dev-only)."
        ),
    }
}

#[cfg(feature = "zkp-arkworks")]
fn build_verifier() -> Arc<dyn referee::zkp::Verifier> {
    use referee::zkp::ArkworksVerifier;
    let vk_bob = std::env::var("REFEREE_VK_BOB_PATH").unwrap_or_default();
    let vk_alice = std::env::var("REFEREE_VK_ALICE_PATH").unwrap_or_default();
    if vk_bob.is_empty() || vk_alice.is_empty() {
        // Verifying keys must come from extro-node's circuit fixtures
        // (one per circuit, BN254 / arkworks-canonical). Without them
        // the verifier cannot validate any proof — a clear startup
        // failure is preferable to silent acceptance.
        panic!(
            "zkp-arkworks feature enabled but REFEREE_VK_BOB_PATH and \
             REFEREE_VK_ALICE_PATH are not set. The verifying keys for \
             Bob's payload-honesty and Alice's signature-honesty circuits \
             are produced by extro-node's circuit definitions. See \
             webycash-server/referee/docs/zkp-circuits.md for the file \
             format. Until those fixtures land, run with mock crypto: \
             remove `--features zkp-arkworks` and set \
             REFEREE_ALLOW_MOCK_CRYPTO=1."
        );
    }
    Arc::new(
        ArkworksVerifier::load_from_files(&vk_bob, &vk_alice)
            .expect("ArkworksVerifier::load_from_files"),
    )
}

#[cfg(not(feature = "zkp-arkworks"))]
fn build_verifier() -> Arc<dyn referee::zkp::Verifier> {
    Arc::new(referee::zkp::MockVerifier::always_ok())
}

#[cfg(feature = "musig2-real")]
fn build_musig_signer() -> Arc<dyn referee::musig2::Musig2Signer> {
    use referee::musig2::RealSigner;
    let key_path = std::env::var("REFEREE_MUSIG2_KEY_PATH").unwrap_or_default();
    if key_path.is_empty() {
        panic!(
            "musig2-real feature enabled but REFEREE_MUSIG2_KEY_PATH is \
             not set. Provide a file path containing the referee's \
             secp256k1 secret share, hex-encoded (32 bytes)."
        );
    }
    Arc::new(RealSigner::load_from_file(&key_path).expect("RealSigner::load_from_file"))
}

#[cfg(not(feature = "musig2-real"))]
fn build_musig_signer() -> Arc<dyn referee::musig2::Musig2Signer> {
    Arc::new(referee::musig2::MockSigner::new())
}
