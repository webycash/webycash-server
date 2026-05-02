//! Boot-time configuration.
//!
//! Loaded from environment variables (and optionally a TOML file). The
//! referee binary refuses to start if any required field is missing —
//! configuration errors must be loud, not silent defaults.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{RefereeError, Result};

/// Top-level config shape.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// `host:port` to bind the HTTP API on.
    pub bind: SocketAddr,

    /// File holding the referee's Ed25519 identity (32-byte raw secret,
    /// hex-encoded). In production, this is a path to a sealed file
    /// pulled from KMS at boot.
    pub identity_key_path: PathBuf,

    /// Base URL of the RGB server we mediate against (mints the
    /// swap-tracking RGB21 record on every swap).
    pub rgb_server_url: String,

    /// Base URL of the Webcash server (typically `https://webcash.org`).
    /// We only call `/api/v1/health_check` on it — never `/replace`.
    pub webcash_server_url: String,

    /// HTTP webhook the referee posts to whenever it needs the push
    /// provider to deliver an `insert_hook` / `invalidate_hook` /
    /// `release-settle` payload to a recipient. Out of scope to run.
    pub push_webhook_url: String,

    /// Shared secret the push provider validates on every webhook call
    /// (HMAC-SHA256 over the canonical request body, `X-Push-HMAC`
    /// header). Path to file with hex-encoded 32-byte key.
    pub push_webhook_hmac_key_path: PathBuf,

    /// Storage backend selection. One of `redis`, `dynamodb`, `fdb`,
    /// or `inmem` (default — dev only). Mirrors the asset-server
    /// `WEBCASH_DB_BACKEND` convention.
    pub db_backend: String,

    /// Redis connection URL (used when `db_backend = "redis"`).
    pub redis_url: Option<String>,

    /// Optional DynamoDB endpoint override (used when
    /// `db_backend = "dynamodb"`). Leave unset for AWS-hosted
    /// DynamoDB; set for DynamoDB Local.
    pub dynamodb_endpoint: Option<String>,

    /// FoundationDB cluster file path (used when `db_backend = "fdb"`).
    pub fdb_cluster_file: Option<String>,

    /// Maximum lifetime of a swap from `initiate` to terminal state. Past
    /// this, the referee aborts and triggers refund. Recommended: 24 h.
    #[serde(default = "default_swap_max_age_secs")]
    pub swap_max_age_secs: u64,

    /// How many `insert_push` retries before giving up and aborting.
    /// Per `docs/referee-zkp-based-swap.md` §4.4, default 3.
    #[serde(default = "default_insert_push_retry")]
    pub insert_push_retry: u8,

    /// Backoff between retries in milliseconds (exponential base).
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
}

fn default_swap_max_age_secs() -> u64 {
    86_400
}

fn default_insert_push_retry() -> u8 {
    3
}

fn default_retry_backoff_ms() -> u64 {
    250
}

impl Config {
    /// Load from process environment.
    ///
    /// Required env vars: `REFEREE_BIND`, `REFEREE_IDENTITY_KEY_PATH`,
    /// `REFEREE_RGB_SERVER_URL`, `REFEREE_WEBCASH_SERVER_URL`,
    /// `REFEREE_PUSH_WEBHOOK_URL`, `REFEREE_PUSH_WEBHOOK_HMAC_KEY_PATH`.
    ///
    /// Optional: `WEBCASH_DB_BACKEND` (default `inmem`),
    /// `REDIS_URL`, `DYNAMODB_ENDPOINT`, `FDB_CLUSTER_FILE`,
    /// `REFEREE_SWAP_MAX_AGE_SECS`, `REFEREE_INSERT_PUSH_RETRY`,
    /// `REFEREE_RETRY_BACKOFF_MS`.
    pub fn from_env() -> Result<Self> {
        let bind = require_env("REFEREE_BIND")?
            .parse()
            .map_err(|e| RefereeError::BadRequest(format!("REFEREE_BIND: {e}")))?;
        let identity_key_path = PathBuf::from(require_env("REFEREE_IDENTITY_KEY_PATH")?);
        let rgb_server_url = require_env("REFEREE_RGB_SERVER_URL")?;
        let webcash_server_url = require_env("REFEREE_WEBCASH_SERVER_URL")?;
        let push_webhook_url = require_env("REFEREE_PUSH_WEBHOOK_URL")?;
        let push_webhook_hmac_key_path =
            PathBuf::from(require_env("REFEREE_PUSH_WEBHOOK_HMAC_KEY_PATH")?);
        let db_backend =
            std::env::var("WEBCASH_DB_BACKEND").unwrap_or_else(|_| "inmem".to_string());
        let redis_url = std::env::var("REDIS_URL").ok();
        let dynamodb_endpoint = std::env::var("DYNAMODB_ENDPOINT").ok();
        let fdb_cluster_file = std::env::var("FDB_CLUSTER_FILE").ok();
        let swap_max_age_secs = std::env::var("REFEREE_SWAP_MAX_AGE_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(default_swap_max_age_secs);
        let insert_push_retry = std::env::var("REFEREE_INSERT_PUSH_RETRY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(default_insert_push_retry);
        let retry_backoff_ms = std::env::var("REFEREE_RETRY_BACKOFF_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(default_retry_backoff_ms);
        Ok(Config {
            bind,
            identity_key_path,
            rgb_server_url,
            webcash_server_url,
            push_webhook_url,
            push_webhook_hmac_key_path,
            db_backend,
            redis_url,
            dynamodb_endpoint,
            fdb_cluster_file,
            swap_max_age_secs,
            insert_push_retry,
            retry_backoff_ms,
        })
    }
}

fn require_env(key: &str) -> Result<String> {
    std::env::var(key).map_err(|_| RefereeError::Internal(format!("env {key} not set")))
}
