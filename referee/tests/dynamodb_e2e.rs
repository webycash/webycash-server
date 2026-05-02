//! DynamoDB-Local end-to-end test for [`DynamoDbSwapStore`].
//!
//! This test is gated on the `REFEREE_DYNAMODB_TEST_URL` env var. To
//! run it locally:
//!
//! ```sh
//! docker run -d --rm -p 8000:8000 amazon/dynamodb-local
//! AWS_ACCESS_KEY_ID=fake AWS_SECRET_ACCESS_KEY=fake AWS_REGION=us-east-1 \
//!     REFEREE_DYNAMODB_TEST_URL=http://localhost:8000 \
//!     cargo test -p referee --features dynamodb --test dynamodb_e2e -- --nocapture
//! ```
//!
//! In CI, wire this up by adding a service container for
//! `amazon/dynamodb-local` and exporting `REFEREE_DYNAMODB_TEST_URL`
//! before running the test job.

#![cfg(feature = "dynamodb")]

use referee::state::{ArkOutpointHash, PgpFingerprint, SwapId, WebcashPublicHash};
use referee::store::{dynamodb::DynamoDbSwapStore, SwapStore};
use referee::transaction::{PartyRole, Transaction, TransactionStatus};

fn skip_if_no_endpoint() -> Option<String> {
    std::env::var("REFEREE_DYNAMODB_TEST_URL").ok()
}

async fn make_store(table_suffix: &str) -> DynamoDbSwapStore {
    let endpoint = skip_if_no_endpoint().expect("env-gated test must check");
    let aws_cfg = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let cfg = aws_sdk_dynamodb::config::Builder::from(&aws_cfg)
        .endpoint_url(endpoint)
        .build();
    let client = aws_sdk_dynamodb::Client::from_conf(cfg);
    let store =
        DynamoDbSwapStore::new(client).with_table(format!("RefereeSwaps-test-{table_suffix}"));
    store.ensure_tables().await.expect("ensure_tables");
    store
}

fn tx(id: &str, bob: &str, alice: &str, phase: &str, created_at: u64) -> Transaction {
    Transaction {
        swap_id: SwapId(id.into()),
        status: TransactionStatus::for_phase(phase),
        phase: phase.into(),
        terminal: TransactionStatus::for_phase(phase).is_terminal(),
        bob_pgp_fp: PgpFingerprint(bob.into()),
        alice_pgp_fp: PgpFingerprint(alice.into()),
        webcash_public_hash: WebcashPublicHash::new("h".repeat(64)),
        vtxo_outpoint_hash: ArkOutpointHash("v".repeat(64)),
        tx_settle_hash: "s".repeat(64),
        tx_refund_hash: "r".repeat(64),
        created_at_unix: created_at,
        updated_at_unix: created_at,
        insert_push_attempts: 0,
        cancel_reason: None,
        canceled_by_pgp_fp: None,
        htlc_refund_contract_id: None,
        state_blob: referee::state::AnyPhaseSwapState {
            phase: phase.into(),
            inner: serde_json::json!({}),
        },
    }
}

#[tokio::test]
async fn dynamodb_upsert_get_and_list_by_party() {
    if skip_if_no_endpoint().is_none() {
        eprintln!("skipping dynamodb_e2e: REFEREE_DYNAMODB_TEST_URL not set");
        return;
    }
    let store = make_store("e2e").await;
    let bob = "bob-fp";
    let alice = "alice-fp";

    store
        .upsert(&tx("a", bob, alice, "init", 1000))
        .await
        .unwrap();
    store
        .upsert(&tx("b", bob, "alice2", "settled", 1500))
        .await
        .unwrap();
    store
        .upsert(&tx("c", "bob2", alice, "refunded", 2000))
        .await
        .unwrap();

    let got = store.get(&SwapId("a".into())).await.unwrap().unwrap();
    assert_eq!(got.phase, "init");
    assert_eq!(got.bob_pgp_fp.0, bob);

    let bob_view = store
        .list_by_party(&PgpFingerprint(bob.into()))
        .await
        .unwrap();
    assert_eq!(bob_view.len(), 2);
    assert_eq!(bob_view[0].swap_id.0, "b"); // newest first by created_at_unix
    for s in &bob_view {
        assert!(matches!(s.role, PartyRole::Bob));
    }

    let alice_view = store
        .list_by_party(&PgpFingerprint(alice.into()))
        .await
        .unwrap();
    assert_eq!(alice_view.len(), 2);

    let none = store
        .list_by_party(&PgpFingerprint("nobody".into()))
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn dynamodb_self_swap_yields_role_both() {
    if skip_if_no_endpoint().is_none() {
        return;
    }
    let store = make_store("self").await;
    let fp = "self-fp";
    store
        .upsert(&tx("only", fp, fp, "init", 1000))
        .await
        .unwrap();
    let view = store
        .list_by_party(&PgpFingerprint(fp.into()))
        .await
        .unwrap();
    assert_eq!(view.len(), 1);
    assert!(matches!(view[0].role, PartyRole::Both));
}
