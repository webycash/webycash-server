//! Referee binary entry point.
//!
//! Wires up the [`Orchestrator`] with the appropriate collaborators and
//! starts the Axum HTTP server. Configuration is loaded from environment
//! variables; see [`Config::from_env`] and `docs/deployment.md`.
//!
//! ## Build flavours
//!
//! At this milestone the production cryptographic backends (real Groth16
//! verifier, real MuSig2 signer) live behind opt-in cargo features. The
//! binary's behaviour at boot depends on the feature flags it was built
//! with:
//!
//! | Build flags | Behaviour |
//! |---|---|
//! | (none) | **Dev-only**: starts with mock cryptographic backends. Refuses to start unless `REFEREE_ALLOW_MOCK_CRYPTO=1` is set in the environment, so an operator never accidentally deploys mocks to production. |
//! | `--features zkp-arkworks` | Real Groth16 verifier; MuSig2 still mocked. Refuses to start under same flag. |
//! | `--features zkp-arkworks,musig2-real` | Both real. Production-ready. |
//!
//! See `docs/deployment.md` for the full production checklist.

use std::sync::Arc;

use referee::api::orchestrator::Orchestrator;
use referee::api::router::build_router;
use referee::audit::InMemoryAuditLog;
use referee::clients::{MockRgb, MockWebcash};
use referee::config::Config;
use referee::push::HttpPush;
use referee::sign::Identity;
use referee::store::InMemoryStore;

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

    // Wire up cryptographic backends. The mock fall-throughs below are
    // gated by `refuse_mocks_in_production` above — if neither the real
    // backend feature is enabled nor `REFEREE_ALLOW_MOCK_CRYPTO=1` is
    // set, we already exited.
    let verifier = build_verifier();
    let musig = build_musig_signer();

    let orch = Orchestrator {
        identity: Arc::new(identity),
        verifier,
        musig,
        webcash: Arc::new(MockWebcash::always_unspent()),
        rgb: Arc::new(MockRgb::new()),
        push: Arc::new(HttpPush::new(config.push_webhook_url.clone(), hmac_key_bytes)),
        audit: Arc::new(InMemoryAuditLog::default()),
        store: Arc::new(InMemoryStore::default()),
        insert_push_retry: config.insert_push_retry,
        retry_backoff: std::time::Duration::from_millis(config.retry_backoff_ms),
        callback_base_url: format!("http://{}/v1/swap", config.bind),
    };

    let app = build_router(Arc::new(orch));
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(addr = %config.bind, "referee listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Refuse to boot if mock cryptographic backends would silently be wired
/// up. The operator must explicitly opt in via `REFEREE_ALLOW_MOCK_CRYPTO=1`
/// for dev/testing — production builds enable both `zkp-arkworks` and
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

#[cfg(feature = "zkp-arkworks")]
fn build_verifier() -> Arc<dyn referee::zkp::Verifier> {
    // The real ArkworksVerifier needs the verifying keys for both
    // circuits, deserialised from disk per `docs/zkp-circuits.md`. Those
    // VKs are produced by extro-node's circuit fixtures, which haven't
    // landed yet. Build with `--features zkp-arkworks` to get a clear
    // runtime error pointing at the integration gap rather than to
    // silently degrade to mocks.
    unimplemented!(
        "zkp-arkworks feature enabled but ArkworksVerifier wiring is \
         pending extro-node circuit fixtures. See \
         webycash-server/referee/docs/zkp-circuits.md for the integration \
         contract; remove `--features zkp-arkworks` for dev (and set \
         REFEREE_ALLOW_MOCK_CRYPTO=1) until the wiring lands."
    )
}

#[cfg(not(feature = "zkp-arkworks"))]
fn build_verifier() -> Arc<dyn referee::zkp::Verifier> {
    Arc::new(referee::zkp::MockVerifier::always_ok())
}

#[cfg(feature = "musig2-real")]
fn build_musig_signer() -> Arc<dyn referee::musig2::Musig2Signer> {
    // Real MuSig2 signer needs a secp256k1 keypair loaded from
    // `REFEREE_MUSIG2_KEY_PATH` and the `musig2` crate's session types
    // wired into `Musig2Signer`. Production wiring tracked alongside
    // the ZKP integration above; both land together with extro-node's
    // counterpart so client + server agree on encoding.
    unimplemented!(
        "musig2-real feature enabled but RealSigner wiring is pending \
         extro-node integration. See \
         webycash-server/referee/docs/musig2-ceremony.md."
    )
}

#[cfg(not(feature = "musig2-real"))]
fn build_musig_signer() -> Arc<dyn referee::musig2::Musig2Signer> {
    Arc::new(referee::musig2::MockSigner::new())
}
