# Implementing a referee client

This doc is for wallet implementors who want to drive a swap through
a referee. It is the contract; everything else (`api.md`,
`hook-contract.md`, `musig2-ceremony.md`) is reference material the
client uses for specific subsystems.

The client lives entirely on the wallet side. The referee is one of
several remote services it talks to — alongside the RGB server,
webcash.org, and the push provider. The wallet implementor owns:

- **PGP** — Bob and Alice each hold a PGP keypair; the wallet
  encrypts to and decrypts on behalf of them.
- **Ed25519 cancel keypair** — independent of PGP; used to
  authenticate `POST /v1/swap/{id}/cancel`. Generated at first use,
  stored locally.
- **MuSig2 secp256k1 keypair** — Alice's share of the 2-of-2 vtxo
  with the referee. Bob doesn't need MuSig2 keys.
- **Groth16 prover** — produces the two payload-honesty proofs (Bob
  encrypts a witness whose hash is `H_B`; Alice encrypts a valid
  partial-sig under her share). Toolchain: extro-node's circuit
  fixtures.
- **Webcash secret material** — Bob's `S_B` lives in his wallet
  only; the referee never sees plaintext.
- **ARK transaction construction** — Alice builds `TX_settle` and
  `TX_refund` against her vtxo and computes their canonical hashes.

What the client does NOT own:

- The webcash leg's `/replace` call. That happens via Alice's
  `insert_hook`-driven webylib integration, after the referee
  delivers Bob's encrypted secret to her PGP fingerprint via the
  push provider.
- ARK ASP integration. The vtxo lifecycle (mint, broadcast, watch)
  is the wallet's job; the referee is told only `tx_settle_hash` /
  `tx_refund_hash` so it can co-sign.
- HTLC RGB record management. The referee mints + closes the
  backup record; the client only consumes its `contract_id` from
  the swap's `Transaction` row.

## Endpoint inventory (v0.3.1)

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/v1/pubkey` | Pin the referee's Ed25519 identity + MuSig2 pubshare. |
| `POST` | `/v1/swap/initiate` | Begin a swap. Returns `swap_id`; row is in `insert-pushed` when the call returns. |
| `POST` | `/v1/swap/{id}/advance` | Schedule one transition. Idempotent. Lambda's scheduler invokes this; clients can also call it for forced progress. |
| `POST` | `/v1/swap/{id}/cancel` | Either party cancels a stuck swap with an Ed25519 signature. |
| `POST` | `/v1/swap/{id}/poll` | Read-only — current status, phase, all top-level row attributes. |
| `GET` | `/v1/swap/{id}/audit` | Full signed audit chain for a swap. |
| `POST` | `/v1/swap/{id}/ack` | Push provider forwards recipient ack receipts (server-internal). |
| `GET` | `/v1/parties/{pgp_fp}/swaps` | Reverse-chronological history (cap 1000) for a party. |

The full request/response shapes are in
`webycash-server/referee/docs/api.md`. Below is the *flow* — the
order of operations, what each side computes, and how the client
authenticates itself.

## Bob's flow (webcash holder, the *seller*)

```
1.  GET  /v1/pubkey                         → pin referee_pubkey, musig2_pubshare
2.  Generate Ed25519 cancel keypair         (one-time per wallet identity)
3.  Receive Alice's offer:
      - alice_pgp_fp + alice_pgp_pubkey_hex + alice_cancel_pubkey_hex
      - alice_musig2_pubkey
      - vtxo + tx_settle_hash + tx_refund_hash
      - alice_nonces (settle + refund pub-nonces, MuSig2)
4.  Pick S_B (random 32 bytes); H_B = sha256(S_B)
5.  Encrypt S_B to Alice's PGP pubkey      → enc_secret_for_alice
6.  Generate Groth16 proof of "this ciphertext, decrypted under
    alice_pgp_pubkey, yields a 32-byte value whose sha256 is H_B"
                                            → zkp_payload
7.  POST /v1/swap/initiate                  → swap_id (referee returns
    after the row is in `insert-pushed`)
8.  Watch /v1/swap/{id}/poll (or GET /v1/parties/{your_fp}/swaps)
    for status transitions:
      pending → settled  (success — proceed to step 9)
      pending → refunded (Alice never picked up; you keep S_B)
      pending → canceled (someone canceled — refund-equivalent)
9.  On `settled`: receive ReleaseSettle push to your PGP fingerprint
    (out-of-band via webylib `insert_hook`). Payload is
    base64({referee_partial_sig, alice_enc_partial_sig}). Decrypt
    alice_enc_partial_sig with your PGP private key to recover
    Alice's MuSig2 partial-sig. Combine with the referee's partial
    to finalise the 2-of-2 signature on TX_settle. Broadcast
    TX_settle to the ARK ASP — the vtxo is now yours.
10. To cancel pre-settle:
      sig = Ed25519.sign(your_cancel_sk,
                         party_cancel_message(swap_id, your_fp, reason))
      POST /v1/swap/{id}/cancel { by_pgp_fp, reason, signature_hex }
```

## Alice's flow (ARK vtxo holder, the *buyer*)

```
1.  GET  /v1/pubkey                         → pin referee_pubkey, musig2_pubshare
2.  Generate Ed25519 cancel keypair         (one-time per wallet identity)
3.  Generate two MuSig2 nonce pairs (settle + refund). Keep secret
    nonces local; publish pub_nonces to Bob.
4.  Build TX_settle and TX_refund against your vtxo (against the
    aggregated 2-of-2 key formed from your_musig2_pubkey +
    referee.musig2_pubshare). Compute their canonical hashes.
5.  Encrypt your TX_settle MuSig2 partial-sig to Bob's PGP pubkey
                                            → enc_partial_sig_for_bob
    (TX_refund partial-sig stays local — never sent to the referee.)
6.  Generate Groth16 proof of "this ciphertext, decrypted under
    bob_pgp_pubkey, yields a valid MuSig2 partial-sig under
    your_musig2_pubkey on TX_settle"        → zkp_signature
7.  Hand the bundle (alice_pgp_*, alice_musig2_pubkey,
    alice_cancel_pubkey_hex, vtxo, tx_settle_hash, tx_refund_hash,
    enc_partial_sig_for_bob, zkp_signature, alice_nonces) to Bob.
8.  Bob calls /v1/swap/initiate. You wait for the `insert_hook` push
    via webylib — payload is base64(enc_secret_for_alice). Decrypt
    with your PGP private key to recover S_B.
9.  Call webcash.org `/replace` with S_B → spend the webcash leg.
    The referee's post-check on H_B sees `Spent` → settle path.
10. On `refunded`: receive ReleaseRefund push to your PGP fingerprint.
    Payload is base64(referee_partial_sig_on_TX_refund). Combine with
    your local TX_refund partial-sig and broadcast TX_refund — you
    get your vtxo back.
11. To cancel pre-`insert-pushed`:
      sig = Ed25519.sign(your_cancel_sk,
                         party_cancel_message(swap_id, your_fp, reason))
      POST /v1/swap/{id}/cancel { by_pgp_fp, reason, signature_hex }

    NOTE: after `insert-pushed`, you cannot unilaterally cancel —
    the referee will refund you automatically once retries exhaust.
```

## Canonical messages the client must produce

### `party_cancel_message`

```
"webycash-referee/cancel-swap-v1:" || swap_id || ":" || by_pgp_fp || ":" || sha256_hex(reason)
```

Signed with the party's `cancel_pubkey_hex` Ed25519 private key.

```rust
// Reference (Rust, matches src/sign.rs::Identity::party_cancel_message)
fn party_cancel_message(swap_id: &str, by_pgp_fp: &str, reason: &str) -> Vec<u8> {
    let reason_hash = hex::encode(sha2::Sha256::digest(reason.as_bytes()));
    format!("webycash-referee/cancel-swap-v1:{swap_id}:{by_pgp_fp}:{reason_hash}").into_bytes()
}
```

### Audit-entry verification

Every entry returned from `GET /v1/swap/{id}/audit` carries:

```json
{
  "swap_id": "…",
  "phase": "init|zkps-verified|pre-checked|insert-pushed|settled|aborted|invalidated|refunded|canceled",
  "ts_unix": 1700000000,
  "prior_tip": "<sha256 hex of previous entry's tip>",
  "phase_payload": { ... per-phase fields ... },
  "signature": "<128 hex Ed25519>"
}
```

To verify:

1. The first entry has `prior_tip == ""`.
2. For each subsequent entry, `prior_tip == sha256_hex(canonical_body(prev))`.
3. For each entry, `Identity::verify(referee_pubkey, tag_for_phase(phase), canonical_body(entry), signature)` succeeds.

Where `tag_for_phase` maps phase → `Tag` (see `src/state/types.rs`)
and the canonical body is the JSON of every field except `signature`,
sorted by key, no whitespace. The reference implementation is
`src/audit/mod.rs::AuditEntry::canonical_body`.

## Polling loop

For wallets without push delivery, poll `/v1/swap/{id}/poll` every
N seconds until `terminal == true`. The response carries the full
`Transaction` row — status, phase, both PGP fingerprints, the webcash
hash + ARK vtxo + tx hashes, timestamps, and the cancel/HTLC fields:

```json
{
  "swap_id": "…",
  "status": "pending|settled|refunded|canceled",
  "phase": "…",
  "terminal": true,
  "bob_pgp_fp": "…",
  "alice_pgp_fp": "…",
  "created_at_unix": 1700000000,
  "updated_at_unix": 1700000300,
  "insert_push_attempts": 1,
  "cancel_reason": null,
  "canceled_by_pgp_fp": null,
  "htlc_refund_contract_id": "rgb-htlc-…"
}
```

Recommended cadence: 5 s while `pending`, exponential backoff to
60 s after `aborted` / `invalidated` (refund path is engaged but may
take a moment). Terminal states never change — stop polling.

## Error mapping

| HTTP | RefereeError variant | Client action |
|---|---|---|
| 400 | `BadRequest` | Fix the request shape; do not retry. |
| 401 | (cancel sig fails) | Cancel signature was invalid. Don't retry. |
| 409 | `InvalidTransition` | The phase forbids this operation. Re-poll for current phase. |
| 422 | `ZkpRejected` / `Musig2` | The proof or signature is invalid. Don't retry — regenerate. |
| 502 | `External` / `Push` | Upstream RGB / webcash.org / push provider hiccup. Retry with backoff. |
| 500 | `Store` / `Crypto` / `Internal` | Server-side; retry, escalate if persistent. |

## Reference Rust client skeleton

A minimal client lives at `webycash-server/referee/examples/`
(future, v0.3.x) — for now, build directly off `reqwest` against the
endpoints listed above. The shapes on the wire match the
`#[derive(Serialize, Deserialize)]` types in
`webycash-server/referee/src/{transaction.rs, state/types.rs,
api/router.rs}`; vendoring those modules is the easiest path.

```rust
// Sketch — a real client should pin the referee pubkey and verify
// every signed response.
use reqwest::Client;

pub struct RefereeClient {
    base: String,
    http: Client,
}

impl RefereeClient {
    pub async fn initiate(&self, req: InitiateRequest) -> Result<InitiateResponse> {
        Ok(self
            .http
            .post(format!("{}/v1/swap/initiate", self.base))
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn cancel(
        &self,
        swap_id: &str,
        by_pgp_fp: &str,
        reason: &str,
        cancel_sk: &ed25519_dalek::SigningKey,
    ) -> Result<()> {
        let body = party_cancel_message(swap_id, by_pgp_fp, reason);
        let sig_hex = hex::encode(cancel_sk.sign(&body).to_bytes());
        self.http
            .post(format!("{}/v1/swap/{swap_id}/cancel", self.base))
            .json(&serde_json::json!({
                "by_pgp_fp": by_pgp_fp,
                "reason": reason,
                "signature_hex": sig_hex,
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn poll(&self, swap_id: &str) -> Result<serde_json::Value> {
        Ok(self
            .http
            .post(format!("{}/v1/swap/{swap_id}/poll", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn history(&self, pgp_fp: &str) -> Result<Vec<TransactionSummary>> {
        Ok(self
            .http
            .get(format!("{}/v1/parties/{pgp_fp}/swaps", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}
```

## Out of scope

- Bilateral cancel ceremony (both parties co-sign a cancel message).
  Currently unilateral pre-`insert-pushed`; bilateral can be layered
  later without a schema change.
- Pagination beyond the 1000-row cap on `/v1/parties/{fp}/swaps`.
- Retention sweep for canceled / settled / refunded rows.
- Multi-region replication / failover patterns.
- Full TypeScript / Python / Go client SDKs — for now, generate
  bindings off the OpenAPI spec at `referee/docs/api.md`.
