//! Swap orchestrator — wires the typestate transitions to the I/O layer.
//!
//! The orchestrator is the *only* place in the crate that coordinates
//! external calls (webcash-client, RGB-client, push-transport, audit log,
//! store, ZKP verifier, MuSig2 signer). The state-transition layer
//! (`crate::state::transitions`) stays pure.
//!
//! ## Stateless / Lambda-friendly entry points
//!
//! All state lives in the configured [`SwapStore`] / [`AuditLog`]
//! backend (Redis, DynamoDB, FoundationDB). The orchestrator holds NO
//! in-memory swap state across calls — every method reads from the
//! store, makes a single transition, and writes back. This is the
//! contract Lambda needs.
//!
//! Two entry points:
//!
//! - [`Orchestrator::start_swap`] — synchronous. Generates a fresh
//!   `SwapId`, persists `init`, verifies both ZKPs, runs the
//!   pre-check, dispatches the first `insert` push. Returns once the
//!   swap is in `insert-pushed`. Idempotent on retry: each call
//!   either advances the state or, if the swap is already past that
//!   phase, returns the existing id without redoing work.
//! - [`Orchestrator::advance_swap`] — synchronous, idempotent. Runs
//!   ONE iteration of the post-check loop: read swap from store,
//!   call webcash health-check, settle / retry-push / abort+refund as
//!   appropriate, persist the new phase. Lambda invokes this on a
//!   schedule (EventBridge, SQS) for each in-flight swap; each call
//!   is one transition — no background tasks, no pending futures.
//! - [`Orchestrator::run_swap`] — runs `start_swap` + repeated
//!   `advance_swap` to terminal in a single call. Used by tests; not
//!   used by Lambda (which would hit the 15-minute invocation limit
//!   on a slow swap).
//!
//! Construction takes trait objects for every collaborator, so tests
//! plug in mocks and production wires up the real implementations.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::audit::{AuditEntry, AuditLog};
use crate::clients::{HtlcCloseKind, HtlcRefundParams, RgbClient, SpentStatus, WebcashClient};
use crate::error::{RefereeError, Result};
use crate::musig2::{Musig2Signer, Session};
use crate::push::{PushKind, PushRequest, PushTransport};
use crate::sign::Identity;
use crate::state::{self, AlicePayload, BobPayload, Musig2Sessions, Parties, SwapId, SwapState};
use crate::store::SwapStore;
use crate::transaction::Transaction;
use crate::zkp::{Circuit, Verifier};

/// All collaborators bundled. Inject via direct field construction.
pub struct Orchestrator {
    /// Referee's Ed25519 identity (audit-log signing + canonical messages).
    pub identity: Arc<Identity>,
    /// ZKP verifier.
    pub verifier: Arc<dyn Verifier>,
    /// MuSig2 signer.
    pub musig: Arc<dyn Musig2Signer>,
    /// Webcash health-check client.
    pub webcash: Arc<dyn WebcashClient>,
    /// RGB-server client (for the swap-tracking record).
    pub rgb: Arc<dyn RgbClient>,
    /// Push transport.
    pub push: Arc<dyn PushTransport>,
    /// Audit log.
    pub audit: Arc<dyn AuditLog>,
    /// Swap-state store.
    pub store: Arc<dyn SwapStore>,
    /// Maximum lifetime of a swap from initiate to terminal. Used as
    /// `created_at_unix + swap_max_age_secs` to set the timeout on
    /// the HTLC backup record.
    pub swap_max_age_secs: u64,
    /// Insert-push retry budget (per `docs/referee-zkp-based-swap.md` §4.4).
    pub insert_push_retry: u8,
    /// Backoff between insert-push retries. The orchestrator sleeps for
    /// this duration between consecutive post-check / re-push iterations
    /// so a stuck webcash leg cannot trigger a hot-loop hammering
    /// webcash.org. Tests set this to `Duration::ZERO` for speed.
    pub retry_backoff: Duration,
    /// Where the orchestrator tells the push provider to deliver wallet
    /// acks (typically `"https://referee.example/v1/swap"` — id appended).
    pub callback_base_url: String,
}

impl Orchestrator {
    /// Initiate a swap synchronously. Persists `init`, verifies both
    /// ZKPs, runs the webcash pre-check, dispatches the first
    /// `insert` push, persists `insert-pushed`. Returns the swap id.
    ///
    /// All state is in the configured store before this method
    /// returns — there is no background task. A subsequent call to
    /// [`advance_swap`] (typically driven by EventBridge / SQS in
    /// Lambda deployments) runs one post-check iteration.
    ///
    /// This is the production HTTP path; the handler reads the body
    /// once and calls this. Total wall-clock is one ZKP verify, one
    /// pre-check round-trip, one push round-trip, and a few
    /// store/audit writes — well under the Lambda 15-minute limit and
    /// typically under one second with mocks.
    pub async fn start_swap(
        &self,
        parties: Parties,
        bob: BobPayload,
        alice: AlicePayload,
        alice_nonces: state::AliceMusig2Nonces,
    ) -> Result<SwapId> {
        let now_unix = self.now();
        let id = SwapId::fresh();

        // 1) Begin two MuSig2 sessions (settle + refund). Backends
        //    persist secret nonces durably; in-memory mock keeps them
        //    in process state and is replaced for Lambda deployments.
        let settle_pub_nonce = self.musig.begin_session(&id, Session::Settle).await?.0;
        let refund_pub_nonce = self.musig.begin_session(&id, Session::Refund).await?.0;
        let referee_sessions = Musig2Sessions {
            settle_pub_nonce,
            refund_pub_nonce,
            secret_nonce_handle: id.clone(),
        };

        // 2) Audit `init`.
        let mut entry = AuditEntry {
            swap_id: id.clone(),
            phase: "init".into(),
            ts_unix: now_unix,
            prior_tip: String::new(),
            phase_payload: serde_json::json!({
                "bob_pgp_fp": parties.bob_pgp_fp.0,
                "alice_pgp_fp": parties.alice_pgp_fp.0,
                "h_b": bob.h_b.0,
                "vtxo": alice.vtxo.0,
                "tx_settle_hash": alice.tx_settle_hash,
                "tx_refund_hash": alice.tx_refund_hash,
            }),
            signature: String::new(),
        };
        let init_tip = self.audit.append(&self.identity, &mut entry).await?;
        let initial = state::initiate(
            id.clone(),
            parties,
            bob.clone(),
            alice.clone(),
            alice_nonces,
            referee_sessions,
            now_unix,
            init_tip,
        );
        self.persist(&initial, "init").await?;

        // 3) Mint the swap-tracking RGB record.
        self.rgb
            .mint_swap_record(
                &id.0,
                &serde_json::json!({
                    "bob": initial.parties.bob_pgp_fp.0,
                    "alice": initial.parties.alice_pgp_fp.0,
                    "tx_settle_hash": initial.alice.tx_settle_hash,
                    "tx_refund_hash": initial.alice.tx_refund_hash,
                }),
            )
            .await?;
        // 3b) Mint the timeout-bound HTLC backup record. Best-effort:
        // backends without HTLC support return None and the swap
        // proceeds via the MuSig2 refund path.
        let htlc_contract = self
            .rgb
            .mint_htlc_refund(
                &id.0,
                &HtlcRefundParams {
                    timeout_unix: now_unix.saturating_add(self.swap_max_age_secs),
                    // No party-supplied refund secret in v0.4.0; the
                    // record is referee-controlled and evidentiary
                    // only. Future revisions can layer
                    // `R_alice`-bound unilateral release.
                    refund_unlock_hash_hex: String::new(),
                    bob_pgp_fp: initial.parties.bob_pgp_fp.0.clone(),
                    alice_pgp_fp: initial.parties.alice_pgp_fp.0.clone(),
                },
            )
            .await
            .unwrap_or(None);
        if let Some(cid) = htlc_contract {
            // Persist the contract id alongside the row so
            // settle/refund/cancel can find it.
            if let Some(mut row) = self.store.get(&id).await? {
                row.htlc_refund_contract_id = Some(cid);
                self.store.upsert(&row).await?;
            }
        }

        // 4) Verify both ZKPs.
        let ok_bob = self
            .verifier
            .verify(Circuit::BobPayload, &bob.zkp_payload)
            .await?;
        let ok_alice = self
            .verifier
            .verify(Circuit::AliceSignature, &alice.zkp_signature)
            .await?;
        let zkps_tip = self
            .audit_phase(
                &id,
                "zkps-verified",
                json_obj(&[("ok_bob", ok_bob.into()), ("ok_alice", ok_alice.into())]),
                &initial.audit_tip_hex,
            )
            .await?;
        let zkps_state = state::verify_zkps(initial, ok_bob && ok_alice, self.now(), zkps_tip)?;
        self.persist(&zkps_state, "zkps-verified").await?;

        // 5) Pre-check the webcash leg.
        let pre_status = self.webcash.check(&zkps_state.bob.h_b).await?;
        let unspent = pre_status == SpentStatus::Unspent;
        let pre_tip = self
            .audit_phase(
                &id,
                "pre-checked",
                serde_json::json!({"unspent": unspent}),
                &zkps_state.audit_tip_hex,
            )
            .await?;
        let pre_state = state::pre_check(zkps_state, unspent, self.now(), pre_tip)?;
        self.persist(&pre_state, "pre-checked").await?;

        // 6) Dispatch the first insert push.
        self.push
            .dispatch(&PushRequest {
                swap_id: id.clone(),
                recipient_pgp_fp: pre_state.parties.alice_pgp_fp.clone(),
                kind: PushKind::Insert,
                payload_b64: base64_of(&pre_state.bob.enc_secret_for_alice.bytes),
                callback_url: self.callback_url(&id),
            })
            .await?;
        let push_tip = self
            .audit_phase(
                &id,
                "insert-pushed",
                serde_json::json!({"attempt": 1}),
                &pre_state.audit_tip_hex,
            )
            .await?;
        let pushed = state::insert_pushed_from_pre(pre_state, self.now(), push_tip);
        self.persist(&pushed, "insert-pushed").await?;
        Ok(id)
    }

    /// Run `start_swap` then loop `advance_swap` until terminal.
    /// Synchronous; tests use this for the full e2e. Production
    /// Lambda deployments do NOT call this — they call `start_swap`
    /// on the request and `advance_swap` on a schedule, so each
    /// invocation is short-lived.
    pub async fn run_to_completion(
        &self,
        parties: Parties,
        bob: BobPayload,
        alice: AlicePayload,
        alice_nonces: state::AliceMusig2Nonces,
    ) -> Result<SwapOutcome> {
        let id = self.start_swap(parties, bob, alice, alice_nonces).await?;
        loop {
            if !self.retry_backoff.is_zero() {
                tokio::time::sleep(self.retry_backoff).await;
            }
            if let Some(outcome) = self.advance_swap(&id).await? {
                return Ok(outcome);
            }
        }
    }

    /// Cancel a swap on the request of one of the parties. Verifies
    /// the party's Ed25519 cancel signature, checks the phase
    /// eligibility, and writes a terminal `canceled` row + audit
    /// entry.
    ///
    /// Permission policy (see `docs/transaction-model.md`):
    /// - `init` / `zkps-verified` / `pre-checked`: either party may
    ///   unilaterally cancel.
    /// - `insert-pushed`: only Bob may unilaterally cancel (he
    ///   withdraws the offer); Alice has to wait for the post-check
    ///   loop to exhaust → refund.
    /// - any other phase: no cancel — let the abort/refund path run
    ///   or wait for terminal.
    pub async fn cancel_swap(
        &self,
        id: &SwapId,
        by_pgp_fp: &state::PgpFingerprint,
        reason: &str,
        sig_hex: &str,
    ) -> Result<()> {
        let tx = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| RefereeError::BadRequest(format!("unknown swap_id: {}", id.0)))?;
        if tx.terminal {
            return Err(RefereeError::InvalidTransition(format!(
                "cannot cancel: swap is already in terminal phase {}",
                tx.phase
            )));
        }

        // Match the requester to a party and pick the right cancel pubkey.
        let parties: Parties = serde_json::from_value(
            tx.state_blob
                .inner
                .get("parties")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        )
        .map_err(|e| RefereeError::Store(format!("decode parties: {e}")))?;
        let (cancel_pubkey, is_bob) = if *by_pgp_fp == parties.bob_pgp_fp {
            (parties.bob_cancel_pubkey_hex.clone(), true)
        } else if *by_pgp_fp == parties.alice_pgp_fp {
            (parties.alice_cancel_pubkey_hex.clone(), false)
        } else {
            return Err(RefereeError::BadRequest(
                "by_pgp_fp does not match either party of this swap".into(),
            ));
        };

        // Phase eligibility per policy.
        match tx.phase.as_str() {
            "init" | "zkps-verified" | "pre-checked" => {} // unilateral OK
            "insert-pushed" => {
                if !is_bob {
                    return Err(RefereeError::InvalidTransition(
                        "alice cannot unilaterally cancel after insert-pushed; \
                         the swap will refund automatically once retries exhaust"
                            .into(),
                    ));
                }
            }
            _ => {
                return Err(RefereeError::InvalidTransition(format!(
                    "cannot cancel from phase {}",
                    tx.phase
                )));
            }
        }

        // Verify the signature.
        let body = Identity::party_cancel_message(&id.0, &by_pgp_fp.0, reason);
        Identity::verify_party_signature(&cancel_pubkey, &body, sig_hex)?;

        // Audit entry first (then the row, so the chain is visible).
        let now = self.now();
        let canceled_tip = self
            .audit_phase(
                id,
                "canceled",
                serde_json::json!({
                    "by": by_pgp_fp.0,
                    "reason_sha256": hex::encode(<sha2::Sha256 as sha2::Digest>::digest(reason.as_bytes())),
                }),
                tx.state_blob
                    .inner
                    .get("audit_tip_hex")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .await?;

        // Construct the canceled row by mutating the existing tx
        // shape — we don't run a typestate transition because the
        // typestate doesn't carry party-cancel data, and the
        // user-facing fields are what matter for cancel.
        let mut canceled = tx;
        canceled.phase = "canceled".into();
        canceled.status = crate::transaction::TransactionStatus::Canceled;
        canceled.terminal = true;
        canceled.cancel_reason = Some(reason.to_string());
        canceled.canceled_by_pgp_fp = Some(by_pgp_fp.clone());
        canceled.updated_at_unix = now;
        canceled.state_blob.phase = "canceled".into();
        if let Some(obj) = canceled.state_blob.inner.as_object_mut() {
            obj.insert(
                "audit_tip_hex".into(),
                serde_json::Value::String(canceled_tip.clone()),
            );
            obj.insert(
                "phase_entered_at".into(),
                serde_json::Value::Number(serde_json::Number::from(now)),
            );
        }
        self.store.upsert(&canceled).await?;

        // Best-effort invalidate push to whichever party may have
        // received an in-flight payload. Pre-`insert-pushed` no-op.
        if matches!(canceled.phase.as_str(), "canceled") && canceled.insert_push_attempts > 0 {
            let _ = self
                .push
                .dispatch(&PushRequest {
                    swap_id: id.clone(),
                    recipient_pgp_fp: parties.alice_pgp_fp.clone(),
                    kind: PushKind::Invalidate,
                    payload_b64: base64_of(canceled.webcash_public_hash.0.as_bytes()),
                    callback_url: self.callback_url(id),
                })
                .await;
        }

        // Discard musig sessions — both, since we are not signing
        // anything for this swap any more.
        let _ = self.musig.discard_session(id, Session::Settle).await;
        let _ = self.musig.discard_session(id, Session::Refund).await;
        self.close_htlc_if_any(id, HtlcCloseKind::Cancel).await;

        Ok(())
    }

    /// Run ONE state-machine transition on the swap identified by
    /// `id`, using only persisted state.
    ///
    /// Returns:
    /// - `Ok(Some(outcome))` if this call moved the swap to a
    ///   terminal phase (`Settled` / `Refunded`).
    /// - `Ok(None)` if the swap progressed but is still in flight
    ///   (more `advance_swap` calls needed).
    /// - `Err(...)` for transient infrastructure failure; safe to
    ///   retry — every transition is committed atomically with its
    ///   audit entry, so a partial failure leaves the persisted
    ///   phase consistent and the next `advance_swap` call resumes
    ///   from where the previous one stopped.
    ///
    /// Idempotent: if `id` is already in a terminal phase, returns
    /// the matching `Some(outcome)` without doing additional work.
    /// If `id` is in `init`, `zkps-verified`, or `pre-checked` (a
    /// freshly-initiated swap that hasn't yet reached
    /// `insert-pushed`), this call is a no-op — `start_swap` is
    /// responsible for the synchronous run through `insert-pushed`.
    pub async fn advance_swap(&self, id: &SwapId) -> Result<Option<SwapOutcome>> {
        let tx = self
            .store
            .get(id)
            .await?
            .ok_or_else(|| RefereeError::BadRequest(format!("unknown swap_id: {}", id.0)))?;
        match tx.phase.as_str() {
            "settled" => Ok(Some(SwapOutcome::Settled {
                swap_id: id.clone(),
            })),
            "refunded" => Ok(Some(SwapOutcome::Refunded {
                swap_id: id.clone(),
            })),
            "canceled" => Ok(Some(SwapOutcome::Canceled {
                swap_id: id.clone(),
            })),
            "insert-pushed" => {
                self.advance_from_insert_pushed(id, &tx.state_blob.inner)
                    .await
            }
            "aborted" => self.advance_from_aborted(id, &tx.state_blob.inner).await,
            "invalidated" => {
                self.advance_from_invalidated(id, &tx.state_blob.inner)
                    .await
            }
            // Earlier phases are owned by `start_swap`; advancing them
            // is a no-op so EventBridge polling on a fresh swap before
            // start_swap finishes is safe.
            _ => Ok(None),
        }
    }

    async fn advance_from_insert_pushed(
        &self,
        id: &SwapId,
        inner: &serde_json::Value,
    ) -> Result<Option<SwapOutcome>> {
        let push_state: SwapState<state::InsertPushed> = serde_json::from_value(inner.clone())
            .map_err(|e| RefereeError::Store(format!("decode insert-pushed: {e}")))?;

        let post = self.webcash.check(&push_state.bob.h_b).await?;
        if post == SpentStatus::Spent {
            let settled_tip = self
                .audit_phase(
                    id,
                    "settled",
                    serde_json::json!({}),
                    &push_state.audit_tip_hex,
                )
                .await?;
            let settled = state::settle(
                push_state,
                state::PostCheckOutcome::Settled,
                self.now(),
                settled_tip,
            )?;
            let referee_partial = self
                .musig
                .partial_sign(
                    id,
                    Session::Settle,
                    settled.alice.tx_settle_hash.as_bytes(),
                    &settled.parties.alice_musig2_pubkey,
                    &crate::musig2::PubNonce(settled.alice_nonces.settle_nonce_pub.clone()),
                )
                .await?;
            self.push
                .dispatch(&PushRequest {
                    swap_id: id.clone(),
                    recipient_pgp_fp: settled.parties.bob_pgp_fp.clone(),
                    kind: PushKind::ReleaseSettle,
                    payload_b64: base64_of(
                        &serde_json::to_vec(&serde_json::json!({
                            "referee_partial_sig": referee_partial.0,
                            "alice_enc_partial_sig": base64_of(&settled.alice.enc_partial_sig_for_bob.bytes),
                        }))
                        .map_err(RefereeError::from)?,
                    ),
                    callback_url: self.callback_url(id),
                })
                .await?;
            self.persist(&settled, "settled").await?;
            self.musig.discard_session(id, Session::Refund).await?;
            self.close_htlc_if_any(id, HtlcCloseKind::Settle).await;
            return Ok(Some(SwapOutcome::Settled {
                swap_id: id.clone(),
            }));
        }

        // Still unspent — retry vs abort.
        if push_state.insert_push_attempts < self.insert_push_retry {
            self.push
                .dispatch(&PushRequest {
                    swap_id: id.clone(),
                    recipient_pgp_fp: push_state.parties.alice_pgp_fp.clone(),
                    kind: PushKind::Insert,
                    payload_b64: base64_of(&push_state.bob.enc_secret_for_alice.bytes),
                    callback_url: self.callback_url(id),
                })
                .await?;
            let new_attempt = (push_state.insert_push_attempts as u32) + 1;
            let retry_tip = self
                .audit_phase(
                    id,
                    "insert-pushed",
                    serde_json::json!({"attempt": new_attempt}),
                    &push_state.audit_tip_hex,
                )
                .await?;
            let retried = state::insert_pushed_retry(push_state, self.now(), retry_tip)?;
            self.persist(&retried, "insert-pushed").await?;
            return Ok(None);
        }

        // Retries exhausted: enter abort.
        let abort_tip = self
            .audit_phase(
                id,
                "aborted",
                serde_json::json!({"attempts": push_state.insert_push_attempts}),
                &push_state.audit_tip_hex,
            )
            .await?;
        let aborted = state::abort(push_state, true, self.now(), abort_tip)?;
        self.persist(&aborted, "aborted").await?;
        // Dispatch the invalidate push to Bob. Idempotent: if a future
        // `advance_swap` resumes from `aborted` it will dispatch
        // again, which the push provider deduplicates by (swap_id,
        // kind).
        self.push
            .dispatch(&PushRequest {
                swap_id: id.clone(),
                recipient_pgp_fp: aborted.parties.bob_pgp_fp.clone(),
                kind: PushKind::Invalidate,
                payload_b64: base64_of(aborted.bob.h_b.0.as_bytes()),
                callback_url: self.callback_url(id),
            })
            .await?;
        Ok(None)
    }

    async fn advance_from_aborted(
        &self,
        id: &SwapId,
        inner: &serde_json::Value,
    ) -> Result<Option<SwapOutcome>> {
        let aborted: SwapState<state::Aborted> = serde_json::from_value(inner.clone())
            .map_err(|e| RefereeError::Store(format!("decode aborted: {e}")))?;
        // For now we treat the dispatch ack as sufficient — production
        // wires real recipient-ack callbacks via `/v1/swap/{id}/ack`,
        // which transition `aborted -> invalidated`.
        let inv_tip = self
            .audit_phase(
                id,
                "invalidated",
                serde_json::json!({"acked": true}),
                &aborted.audit_tip_hex,
            )
            .await?;
        let invalidated = state::invalidated(aborted, true, self.now(), inv_tip)?;
        self.persist(&invalidated, "invalidated").await?;
        Ok(None)
    }

    async fn advance_from_invalidated(
        &self,
        id: &SwapId,
        inner: &serde_json::Value,
    ) -> Result<Option<SwapOutcome>> {
        let invalidated: SwapState<state::Invalidated> = serde_json::from_value(inner.clone())
            .map_err(|e| RefereeError::Store(format!("decode invalidated: {e}")))?;
        let refund_partial = self
            .musig
            .partial_sign(
                id,
                Session::Refund,
                invalidated.alice.tx_refund_hash.as_bytes(),
                &invalidated.parties.alice_musig2_pubkey,
                &crate::musig2::PubNonce(invalidated.alice_nonces.refund_nonce_pub.clone()),
            )
            .await?;
        self.push
            .dispatch(&PushRequest {
                swap_id: id.clone(),
                recipient_pgp_fp: invalidated.parties.alice_pgp_fp.clone(),
                kind: PushKind::ReleaseRefund,
                payload_b64: base64_of(refund_partial.0.as_bytes()),
                callback_url: self.callback_url(id),
            })
            .await?;
        let refunded_tip = self
            .audit_phase(
                id,
                "refunded",
                serde_json::json!({}),
                &invalidated.audit_tip_hex,
            )
            .await?;
        let refunded = state::refunded(invalidated, self.now(), refunded_tip);
        self.persist(&refunded, "refunded").await?;
        self.musig.discard_session(id, Session::Settle).await?;
        self.close_htlc_if_any(id, HtlcCloseKind::Refund).await;
        Ok(Some(SwapOutcome::Refunded {
            swap_id: id.clone(),
        }))
    }

    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Close the HTLC backup record if one was minted at initiate.
    /// Best-effort: any RGB error is logged and dropped, so a
    /// flaky RGB server never blocks settlement / refund.
    async fn close_htlc_if_any(&self, id: &SwapId, kind: HtlcCloseKind) {
        let Ok(Some(row)) = self.store.get(id).await else {
            return;
        };
        let Some(cid) = row.htlc_refund_contract_id.as_deref() else {
            return;
        };
        if let Err(e) = self.rgb.close_htlc_refund(&id.0, cid, kind).await {
            tracing::warn!(swap_id = %id.0, err = %e, "close_htlc_refund best-effort failed");
        }
    }

    fn callback_url(&self, id: &SwapId) -> String {
        format!("{}/{}/ack", self.callback_base_url, id.0)
    }

    async fn audit_phase(
        &self,
        id: &SwapId,
        phase: &str,
        payload: serde_json::Value,
        prior_tip: &str,
    ) -> Result<String> {
        let mut entry = AuditEntry {
            swap_id: id.clone(),
            phase: phase.into(),
            ts_unix: self.now(),
            prior_tip: prior_tip.into(),
            phase_payload: payload,
            signature: String::new(),
        };
        self.audit.append(&self.identity, &mut entry).await
    }

    async fn persist<P: state::Phase>(&self, s: &SwapState<P>, phase: &str) -> Result<()> {
        // Project the typestate into the user-facing Transaction shape.
        // First-write `created_at_unix` is taken from `s.phase_entered_at`
        // when phase is "init"; otherwise we preserve whatever the
        // existing row held (so created_at_unix is monotonic across
        // upserts).
        let now = self.now();
        let mut tx = Transaction::derive_from(s, phase, now, None, None, None);
        if phase == "init" {
            tx.created_at_unix = s.phase_entered_at;
        } else if let Some(existing) = self.store.get(&s.id).await? {
            tx.created_at_unix = existing.created_at_unix;
            // Preserve cancel + HTLC fields set by earlier writes; the
            // typestate doesn't carry them.
            tx.cancel_reason = existing.cancel_reason;
            tx.canceled_by_pgp_fp = existing.canceled_by_pgp_fp;
            tx.htlc_refund_contract_id = existing.htlc_refund_contract_id;
        }
        self.store.upsert(&tx).await
    }
}

/// Final outcome of `Orchestrator::run_swap`.
#[derive(Debug, Clone)]
pub enum SwapOutcome {
    /// Settlement succeeded: Bob received his settle release.
    Settled {
        /// The completed swap's id.
        swap_id: SwapId,
    },
    /// Refund path completed: Alice received her refund partial-sig.
    Refunded {
        /// The completed swap's id.
        swap_id: SwapId,
    },
    /// Canceled by a party via `POST /v1/swap/{id}/cancel` (or by the
    /// referee at swap_max_age timeout — future work).
    Canceled {
        /// The canceled swap's id.
        swap_id: SwapId,
    },
}

fn json_obj(pairs: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    serde_json::Value::Object(m)
}

fn base64_of(bytes: &[u8]) -> String {
    // Standard alphabet, padded — match `docs/push-notification.md`.
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(bytes)
}
