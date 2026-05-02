//! Axum router wiring.
//!
//! Builds the HTTP surface from a fully-constructed [`Orchestrator`].
//! Every endpoint consults the same orchestrator instance; testability is
//! achieved by constructing the orchestrator with mock collaborators.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::api::orchestrator::Orchestrator;
use crate::error::RefereeError;
use crate::state::{
    AliceMusig2Nonces, AlicePayload, BobPayload, Parties, SwapId,
};

/// Build the full `/v1` router from a constructed orchestrator.
pub fn build_router(orch: Arc<Orchestrator>) -> Router {
    Router::new()
        .route("/v1/pubkey", get(pubkey))
        .route("/v1/swap/initiate", post(initiate))
        .route("/v1/swap/:id/audit", get(audit_for))
        .route("/v1/swap/:id/poll", post(poll_status))
        .route("/v1/swap/:id/ack", post(ack))
        .with_state(orch)
}

#[derive(Debug, Serialize)]
struct PubkeyResponse {
    /// Hex Ed25519 identity pubkey.
    ed25519_pubkey_hex: String,
    /// Hex 33-byte secp256k1 MuSig2 pubshare.
    musig2_pubshare_hex: String,
    /// Crate version.
    referee_version: &'static str,
}

async fn pubkey(State(orch): State<Arc<Orchestrator>>) -> impl IntoResponse {
    let resp = PubkeyResponse {
        ed25519_pubkey_hex: orch.identity.pubkey_hex(),
        musig2_pubshare_hex: orch.musig.pubshare().0,
        referee_version: crate::VERSION,
    };
    Json(resp)
}

#[derive(Debug, Deserialize)]
struct InitiateRequest {
    parties: Parties,
    bob: BobPayload,
    alice: AlicePayload,
    alice_nonces: AliceMusig2Nonces,
}

#[derive(Debug, Serialize)]
struct InitiateResponse {
    swap_id: String,
    /// Always `"accepted"` from this endpoint — orchestration is spawned
    /// in the background and progresses through `init → zkps-verified →
    /// pre-checked → insert-pushed → settled|refunded`. Clients poll
    /// `GET /v1/swap/{id}/poll` for terminal phase.
    status: &'static str,
}

async fn initiate(
    State(orch): State<Arc<Orchestrator>>,
    Json(req): Json<InitiateRequest>,
) -> Result<Json<InitiateResponse>, ApiError> {
    let id = orch
        .start_swap(req.parties, req.bob, req.alice, req.alice_nonces)
        .await
        .map_err(ApiError)?;
    Ok(Json(InitiateResponse {
        swap_id: id.0,
        status: "accepted",
    }))
}

async fn audit_for(
    State(orch): State<Arc<Orchestrator>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::audit::AuditEntry>>, ApiError> {
    let entries = orch.audit.entries_for(&SwapId(id)).await.map_err(ApiError)?;
    Ok(Json(entries))
}

async fn poll_status(
    State(orch): State<Arc<Orchestrator>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row = orch
        .store
        .get(&SwapId(id))
        .await
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(RefereeError::BadRequest("unknown swap_id".into())))?;
    Ok(Json(serde_json::json!({
        "phase": row.state.phase,
        "updated_at_unix": row.updated_at_unix,
    })))
}

/// Body of `POST /v1/swap/{id}/ack` — the recipient wallet's ack
/// receipt forwarded by the push provider.
#[derive(Debug, Deserialize)]
pub struct AckRequest {
    /// What was acknowledged (matches `PushKind`).
    pub kind: String,
    /// Recipient's signed receipt (Ed25519, hex).
    pub receipt_sig_hex: String,
}

async fn ack(
    State(_orch): State<Arc<Orchestrator>>,
    Path(_id): Path<String>,
    Json(req): Json<AckRequest>,
) -> impl IntoResponse {
    // Ack is consumed only on the abort path's `invalidate` step in the
    // current run-swap loop. Future expansion (per docs/trust-model.md
    // "Future work — receipt-bound state machine"): store the receipt
    // and gate refund-send on an explicit ack. For now we simply accept
    // and trace; auditors can verify receipts against logged signatures.
    tracing::debug!(kind = %req.kind, "ack received");
    StatusCode::OK
}

// ─────────────────────────────────────────────────────────────────────────────
// Error mapping
// ─────────────────────────────────────────────────────────────────────────────

struct ApiError(RefereeError);

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match &self.0 {
            RefereeError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            RefereeError::ZkpRejected(m) => (StatusCode::UNPROCESSABLE_ENTITY, m.clone()),
            RefereeError::Musig2(m) => (StatusCode::UNPROCESSABLE_ENTITY, m.clone()),
            RefereeError::InvalidTransition(m) => (StatusCode::CONFLICT, m.clone()),
            RefereeError::Store(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
            RefereeError::External(m) => (StatusCode::BAD_GATEWAY, m.clone()),
            RefereeError::Push(m) => (StatusCode::BAD_GATEWAY, m.clone()),
            RefereeError::Crypto(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
            RefereeError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };
        (
            status,
            Json(serde_json::json!({"error": msg, "kind": format!("{:?}", self.0)})),
        )
            .into_response()
    }
}
