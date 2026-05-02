//! Orchestrator end-to-end tests with mocks.
//!
//! No external services. Every collaborator is a mock from
//! `referee::{zkp::MockVerifier, musig2::MockSigner, push::MockPush,
//! clients::{MockWebcash, MockRgb}, audit::InMemoryAuditLog,
//! store::InMemoryStore}`. Drives `Orchestrator::run_swap` through:
//!
//! 1. **Settlement happy path** — pre-check unspent → insert push →
//!    post-check spent → release-settle delivered to Bob.
//! 2. **Abort path** — retries exhausted → invalidate push delivered to
//!    Bob → release-refund delivered to Alice.
//! 3. **ZKP rejection** — verifier rejects Bob's payload-honesty proof.
//! 4. **Audit log integrity** — every phase produces a signed entry, and
//!    each entry's `prior_tip` matches the previous entry's `tip_hash`.
//! 5. **Pre-check fails** — webcash hash already spent before insert.

use std::sync::Arc;
use std::time::Duration;

use referee::api::orchestrator::{Orchestrator, SwapOutcome};
use referee::audit::{AuditLog, InMemoryAuditLog};
use referee::clients::{MockRgb, MockWebcash, SpentStatus};
use referee::error::RefereeError;
use referee::musig2::MockSigner;
use referee::push::{MockPush, PushKind};
use referee::sign::{Identity, Tag};
use referee::state::{
    tag_for_phase, AliceMusig2Nonces, AlicePayload, ArkOutpointHash, BobPayload, Groth16Proof,
    Parties, PgpEncrypted, PgpFingerprint, Secp256k1Pubkey, WebcashPublicHash,
};
use referee::store::{InMemoryStore, SwapStore};
use referee::zkp::MockVerifier;

// ─────────────────────────────────────────────────────────────────────────────
// Shared fixtures
// ─────────────────────────────────────────────────────────────────────────────

fn parties() -> Parties {
    Parties {
        bob_pgp_fp: PgpFingerprint("bb".repeat(20)),
        bob_pgp_pubkey_hex: "ee".repeat(64),
        alice_pgp_fp: PgpFingerprint("aa".repeat(20)),
        alice_pgp_pubkey_hex: "ff".repeat(64),
        alice_musig2_pubkey: Secp256k1Pubkey(format!("02{}", "11".repeat(32))),
        bob_cancel_pubkey_hex: "11".repeat(32),
        alice_cancel_pubkey_hex: "22".repeat(32),
    }
}

fn bob_payload() -> BobPayload {
    BobPayload {
        h_b: WebcashPublicHash::new("h".repeat(64)),
        enc_secret_for_alice: PgpEncrypted::new(b"<encrypted webcash secret to alice>".to_vec()),
        zkp_payload: Groth16Proof {
            proof: vec![1; 64],
            public_inputs: vec![vec![0xa; 32]],
        },
    }
}

fn alice_payload() -> AlicePayload {
    AlicePayload {
        vtxo: ArkOutpointHash("v".repeat(64)),
        tx_settle_hash: "s".repeat(64),
        tx_refund_hash: "r".repeat(64),
        enc_partial_sig_for_bob: PgpEncrypted::new(
            b"<encrypted alice partial-sig to bob>".to_vec(),
        ),
        zkp_signature: Groth16Proof {
            proof: vec![2; 64],
            public_inputs: vec![vec![0xb; 32]],
        },
    }
}

fn nonces() -> AliceMusig2Nonces {
    AliceMusig2Nonces {
        settle_nonce_pub: "11".repeat(66),
        refund_nonce_pub: "22".repeat(66),
    }
}

struct Harness {
    orch: Orchestrator,
    push: Arc<MockPush>,
    audit: Arc<InMemoryAuditLog>,
    store: Arc<InMemoryStore>,
    rgb: Arc<MockRgb>,
    identity: Arc<Identity>,
}

fn harness_with(
    verifier: Arc<dyn referee::zkp::Verifier>,
    webcash: Arc<dyn referee::clients::WebcashClient>,
    retries: u8,
) -> Harness {
    let identity = Arc::new(Identity::from_secret_bytes([7; 32]));
    let push = Arc::new(MockPush::new());
    let audit = Arc::new(InMemoryAuditLog::default());
    let store = Arc::new(InMemoryStore::default());
    let rgb = Arc::new(MockRgb::new());
    let musig = Arc::new(MockSigner::new());

    let orch = Orchestrator {
        identity: identity.clone(),
        verifier,
        musig,
        webcash,
        rgb: rgb.clone(),
        push: push.clone(),
        audit: audit.clone(),
        store: store.clone(),
        swap_max_age_secs: 86_400,
        insert_push_retry: retries,
        // Tests run with no backoff so the post-check loop iterates as
        // fast as the mocks return; production sets this from
        // `Config::retry_backoff_ms`.
        retry_backoff: Duration::ZERO,
        callback_base_url: "http://test/v1/swap".into(),
    };
    Harness {
        orch,
        push,
        audit,
        store,
        rgb,
        identity,
    }
}

/// Drive `start_swap` + repeated `advance_swap` to terminal. Tests
/// use this directly so assertions can inspect every push, audit
/// entry, and store row deterministically — production (Lambda)
/// splits the same flow across multiple invocations.
async fn run_swap(
    h: &Harness,
    parties: Parties,
    bob: BobPayload,
    alice: AlicePayload,
    nonces: AliceMusig2Nonces,
) -> Result<SwapOutcome, RefereeError> {
    h.orch.run_to_completion(parties, bob, alice, nonces).await
}

// ─────────────────────────────────────────────────────────────────────────────
// 1. Settlement happy path
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn settlement_happy_path_delivers_release_settle_to_bob() {
    // Pre-check: Unspent. Post-check: Spent. Both ZKPs pass.
    let webcash = Arc::new(MockWebcash::scripted(
        SpentStatus::Spent,
        vec![SpentStatus::Unspent, SpentStatus::Spent],
    ));
    let h = harness_with(Arc::new(MockVerifier::always_ok()), webcash, 3);

    let outcome = run_swap(&h, parties(), bob_payload(), alice_payload(), nonces())
        .await
        .expect("orchestrator run");
    assert!(matches!(outcome, SwapOutcome::Settled { .. }));

    // Pushes: insert_hook to Alice, then release_settle to Bob.
    let pushes = h.push.snapshot();
    assert_eq!(pushes.len(), 2, "expected 2 pushes, got: {pushes:#?}");
    assert!(matches!(pushes[0].kind, PushKind::Insert));
    assert_eq!(pushes[0].recipient_pgp_fp, parties().alice_pgp_fp);
    assert!(matches!(pushes[1].kind, PushKind::ReleaseSettle));
    assert_eq!(pushes[1].recipient_pgp_fp, parties().bob_pgp_fp);

    // RGB record minted exactly once.
    assert_eq!(h.rgb.calls.lock().await.len(), 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Abort path — retries exhausted → invalidate → refund
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn abort_path_invalidates_bob_then_refunds_alice() {
    // Pre-check: unspent. Every post-check: still unspent → triggers retries
    // until exhausted.
    let webcash = Arc::new(MockWebcash::scripted(
        SpentStatus::Unspent,
        vec![SpentStatus::Unspent; 16], // never goes spent
    ));
    let h = harness_with(Arc::new(MockVerifier::always_ok()), webcash, 2);

    let outcome = run_swap(&h, parties(), bob_payload(), alice_payload(), nonces())
        .await
        .expect("orchestrator run");
    assert!(matches!(outcome, SwapOutcome::Refunded { .. }));

    let pushes = h.push.snapshot();
    // 1 initial insert + 2 retries (insert_push_retry=2) + invalidate + refund.
    let kinds: Vec<_> = pushes.iter().map(|p| p.kind).collect();
    assert!(
        kinds
            .iter()
            .filter(|k| matches!(k, PushKind::Insert))
            .count()
            >= 2,
        "expected at least 2 insert pushes, got {kinds:?}"
    );
    assert!(
        kinds.contains(&PushKind::Invalidate),
        "expected an Invalidate push, got {kinds:?}"
    );
    assert!(
        kinds.contains(&PushKind::ReleaseRefund),
        "expected a ReleaseRefund push, got {kinds:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. ZKP rejection
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn zkp_rejected_short_circuits_before_pre_check() {
    let h = harness_with(
        Arc::new(MockVerifier::with_outcomes(vec![false, true])),
        Arc::new(MockWebcash::always_unspent()),
        3,
    );

    let err = run_swap(&h, parties(), bob_payload(), alice_payload(), nonces())
        .await
        .expect_err("must reject");
    assert!(matches!(err, RefereeError::ZkpRejected(_)));

    // No push should have been dispatched (we short-circuit).
    let pushes = h.push.snapshot();
    assert!(
        pushes.is_empty(),
        "no pushes expected on ZKP rejection, got: {pushes:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Audit log integrity
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn audit_log_chain_is_well_formed_on_happy_path() {
    let webcash = Arc::new(MockWebcash::scripted(
        SpentStatus::Spent,
        vec![SpentStatus::Unspent, SpentStatus::Spent],
    ));
    let h = harness_with(Arc::new(MockVerifier::always_ok()), webcash, 3);
    let outcome = run_swap(&h, parties(), bob_payload(), alice_payload(), nonces())
        .await
        .unwrap();
    let SwapOutcome::Settled { swap_id } = outcome else {
        panic!("expected settled");
    };

    let entries = h.audit.entries_for(&swap_id).await.unwrap();
    assert!(
        entries.len() >= 5,
        "expected init+zkps+pre+insert+settled (≥5), got {}",
        entries.len()
    );
    let phases: Vec<_> = entries.iter().map(|e| e.phase.as_str()).collect();
    assert_eq!(phases[0], "init");
    assert_eq!(phases[1], "zkps-verified");
    assert_eq!(phases[2], "pre-checked");
    assert_eq!(phases[3], "insert-pushed");
    assert_eq!(phases.last().copied(), Some("settled"));

    // Chain: each entry's prior_tip must match the previous entry's tip_hash,
    // except the first which has empty prior_tip.
    assert_eq!(entries[0].prior_tip, "");
    for i in 1..entries.len() {
        assert_eq!(
            entries[i].prior_tip,
            entries[i - 1].tip_hash(),
            "audit chain broken at entry {i}"
        );
    }

    // Every entry has a valid Ed25519 signature under the referee's
    // pubkey, signed against the canonical message for its phase tag.
    let pubkey = h.identity.pubkey();
    for (i, e) in entries.iter().enumerate() {
        assert_eq!(e.signature.len(), 128, "expected 64-byte ed25519 sig hex");
        let tag: Tag = tag_for_phase(&e.phase);
        Identity::verify(pubkey, tag, &e.canonical_body(), &e.signature)
            .unwrap_or_else(|err| panic!("audit entry {i} ({}) failed verify: {err}", e.phase));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 5. Pre-check failure — webcash hash already spent before insert
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn pre_check_already_spent_short_circuits() {
    // Verifier: ok. Webcash: pre-check returns Spent immediately.
    let webcash = Arc::new(MockWebcash::scripted(
        SpentStatus::Spent,
        vec![SpentStatus::Spent],
    ));
    let h = harness_with(Arc::new(MockVerifier::always_ok()), webcash, 3);

    let err = run_swap(&h, parties(), bob_payload(), alice_payload(), nonces())
        .await
        .expect_err("must reject");
    assert!(matches!(err, RefereeError::InvalidTransition(_)));

    // No insert / settle / refund pushes — only the init RGB record was minted.
    let pushes = h.push.snapshot();
    assert!(
        pushes.is_empty(),
        "no pushes expected on pre-check rejection: {pushes:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 6. Persistence: every phase upserts a row into the store
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn store_reflects_terminal_phase_on_settled_path() {
    let webcash = Arc::new(MockWebcash::scripted(
        SpentStatus::Spent,
        vec![SpentStatus::Unspent, SpentStatus::Spent],
    ));
    let h = harness_with(Arc::new(MockVerifier::always_ok()), webcash, 3);
    let outcome = run_swap(&h, parties(), bob_payload(), alice_payload(), nonces())
        .await
        .unwrap();
    let SwapOutcome::Settled { swap_id } = outcome else {
        panic!("expected settled");
    };
    let row = h.store.get(&swap_id).await.unwrap().expect("row exists");
    assert_eq!(row.phase, "settled");
    assert!(row.terminal);
    assert_eq!(row.status, referee::transaction::TransactionStatus::Settled);
}

#[tokio::test]
async fn store_reflects_terminal_phase_on_refunded_path() {
    let webcash = Arc::new(MockWebcash::scripted(
        SpentStatus::Unspent,
        vec![SpentStatus::Unspent; 16],
    ));
    let h = harness_with(Arc::new(MockVerifier::always_ok()), webcash, 1);
    let outcome = run_swap(&h, parties(), bob_payload(), alice_payload(), nonces())
        .await
        .unwrap();
    let SwapOutcome::Refunded { swap_id } = outcome else {
        panic!("expected refunded");
    };
    let row = h.store.get(&swap_id).await.unwrap().expect("row exists");
    assert_eq!(row.phase, "refunded");
    assert!(row.terminal);
    assert_eq!(
        row.status,
        referee::transaction::TransactionStatus::Refunded
    );
}
