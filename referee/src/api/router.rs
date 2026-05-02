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
use crate::state::{AliceMusig2Nonces, AlicePayload, BobPayload, Parties, PgpFingerprint, SwapId};
use crate::transaction::TransactionSummary;

/// Build the full `/v1` router from a constructed orchestrator.
pub fn build_router(orch: Arc<Orchestrator>) -> Router {
    Router::new()
        .route("/v1/pubkey", get(pubkey))
        .route("/v1/swap/initiate", post(initiate))
        .route("/v1/swap/:id/advance", post(advance))
        .route("/v1/swap/:id/cancel", post(cancel))
        .route("/v1/swap/:id/audit", get(audit_for))
        .route("/v1/swap/:id/poll", post(poll_status))
        .route("/v1/swap/:id/ack", post(ack))
        .route("/v1/parties/:fp/swaps", get(party_swaps))
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
    /// Phase the swap is in when this response is returned. By the
    /// time `start_swap` finishes, the swap is in `insert-pushed`
    /// (init → zkps-verified → pre-checked → insert-pushed all run
    /// synchronously inside the handler — no background work).
    /// Subsequent transitions happen via `POST
    /// /v1/swap/{id}/advance`, which a Lambda scheduler invokes on a
    /// cadence.
    phase: &'static str,
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
        phase: "insert-pushed",
    }))
}

/// Body of `POST /v1/swap/{id}/advance`. Empty for now; future
/// versions may include an `as_of` timestamp for the scheduler to
/// detect drift.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct AdvanceRequest {}

#[derive(Debug, Serialize)]
struct AdvanceResponse {
    swap_id: String,
    phase: String,
    terminal: bool,
}

/// Run one state-machine transition on the swap. Idempotent: a
/// scheduler can invoke this on a cadence; once the swap is
/// terminal further calls return the terminal phase without
/// dispatching duplicate side-effects.
async fn advance(
    State(orch): State<Arc<Orchestrator>>,
    Path(id): Path<String>,
    Json(_req): Json<AdvanceRequest>,
) -> Result<Json<AdvanceResponse>, ApiError> {
    let swap_id = SwapId(id.clone());
    let outcome = orch.advance_swap(&swap_id).await.map_err(ApiError)?;
    let tx = orch
        .store
        .get(&swap_id)
        .await
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(RefereeError::BadRequest("unknown swap_id".into())))?;
    Ok(Json(AdvanceResponse {
        swap_id: id,
        phase: tx.phase,
        terminal: outcome.is_some(),
    }))
}

async fn audit_for(
    State(orch): State<Arc<Orchestrator>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<crate::audit::AuditEntry>>, ApiError> {
    let entries = orch
        .audit
        .entries_for(&SwapId(id))
        .await
        .map_err(ApiError)?;
    Ok(Json(entries))
}

async fn poll_status(
    State(orch): State<Arc<Orchestrator>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tx = orch
        .store
        .get(&SwapId(id))
        .await
        .map_err(ApiError)?
        .ok_or_else(|| ApiError(RefereeError::BadRequest("unknown swap_id".into())))?;
    Ok(Json(serde_json::json!({
        "swap_id": tx.swap_id.0,
        "status": tx.status.as_str(),
        "phase": tx.phase,
        "terminal": tx.terminal,
        "bob_pgp_fp": tx.bob_pgp_fp.0,
        "alice_pgp_fp": tx.alice_pgp_fp.0,
        "created_at_unix": tx.created_at_unix,
        "updated_at_unix": tx.updated_at_unix,
        "insert_push_attempts": tx.insert_push_attempts,
        "cancel_reason": tx.cancel_reason,
        "canceled_by_pgp_fp": tx.canceled_by_pgp_fp,
        "htlc_refund_contract_id": tx.htlc_refund_contract_id,
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

/// Body of `POST /v1/swap/{id}/cancel`. The signature authenticates
/// the request as coming from the named party — see
/// `docs/transaction-model.md` §Cancellation for the canonical
/// signed-message format.
#[derive(Debug, Deserialize)]
pub struct CancelRequest {
    /// Which party is asking to cancel — must equal `bob_pgp_fp` or
    /// `alice_pgp_fp` in the swap row.
    pub by_pgp_fp: String,
    /// Free-text reason; only its sha256 is signed and recorded in
    /// the audit log, so it can be long without inflating the chain.
    pub reason: String,
    /// Hex of the 64-byte Ed25519 signature over
    /// `Identity::party_cancel_message(swap_id, by_pgp_fp, reason)`,
    /// signed with the party's `cancel_pubkey_hex` registered at
    /// initiate.
    pub signature_hex: String,
}

#[derive(Debug, Serialize)]
struct CancelResponse {
    swap_id: String,
    phase: &'static str,
    terminal: bool,
}

async fn cancel(
    State(orch): State<Arc<Orchestrator>>,
    Path(id): Path<String>,
    Json(req): Json<CancelRequest>,
) -> Result<Json<CancelResponse>, ApiError> {
    let swap_id = SwapId(id.clone());
    let by = PgpFingerprint(req.by_pgp_fp);
    orch.cancel_swap(&swap_id, &by, &req.reason, &req.signature_hex)
        .await
        .map_err(ApiError)?;
    Ok(Json(CancelResponse {
        swap_id: id,
        phase: "canceled",
        terminal: true,
    }))
}

/// `GET /v1/parties/{pgp_fp}/swaps` — every transaction the
/// fingerprint participated in (as Bob, as Alice, or as both),
/// newest first, capped at 1000.
async fn party_swaps(
    State(orch): State<Arc<Orchestrator>>,
    Path(fp): Path<String>,
) -> Result<Json<Vec<TransactionSummary>>, ApiError> {
    let summaries = orch
        .store
        .list_by_party(&PgpFingerprint(fp))
        .await
        .map_err(ApiError)?;
    Ok(Json(summaries))
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
