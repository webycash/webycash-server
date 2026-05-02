# Transaction model

The referee is fully stateless. Every byte of state lives in the
configured backend (DynamoDB / Redis / FoundationDB). This document
defines the persisted shape — what is stored, what is queryable, and
what the user-facing semantics are.

## Goals

1. **Minimum data, beautifully modeled.** No opaque blobs as
   first-class persisted state. Top-level fields are explicit so
   operators can read a row and immediately understand it.
2. **PGP-fingerprint-indexed history.** Bob and Alice each get a
   queryable list of the swaps they participated in, by their PGP
   fingerprint, with the current status of each.
3. **Cancellation.** Either party can cancel a stuck swap, with
   bounded permission (unilateral pre-`insert-pushed`, mutual or
   timeout-bound after).
4. **HTLC backup refund.** The referee mints a timeout-bound HTLC
   record on the RGB server at initiate so an abort path leaves Alice
   with an on-chain evidentiary trail independent of the referee's
   continued availability.
5. **Lambda-friendly.** Every read and write is a single round-trip
   on the chosen backend. No cross-row transactions, no in-memory
   caches.

## The `Transaction` row

One row per swap. Persisted as explicit attributes (DynamoDB), a JSON
hash (Redis), or a tuple-key entry (FoundationDB).

| Field | Type | Notes |
|---|---|---|
| `swap_id` | string (UUID) | Stable id assigned at `/v1/swap/initiate`. Partition key on every backend. |
| `status` | enum | `pending` / `settled` / `refunded` / `canceled` |
| `phase` | string | Detailed typestate phase: `init` / `zkps-verified` / `pre-checked` / `insert-pushed` / `settled` / `aborted` / `invalidated` / `refunded` / `canceled`. Always more specific than `status`. |
| `terminal` | bool | `true` when `status ∈ {settled, refunded, canceled}`. Derived but stored to avoid recomputation when listing. |
| `bob_pgp_fp` | string (40 hex) | Indexed via GSI on DynamoDB; auxiliary set on Redis; subspace on FDB. |
| `alice_pgp_fp` | string (40 hex) | Indexed alongside `bob_pgp_fp`. |
| `webcash_public_hash` | string (64 hex) | `H_B = sha256(S_B)` — the public hash on the webcash leg. |
| `vtxo_outpoint_hash` | string (64 hex) | Hash of the ARK vtxo being mediated. |
| `tx_settle_hash` | string (64 hex) | What Alice's MuSig2 partial signs over on the settle path. |
| `tx_refund_hash` | string (64 hex) | What Alice's MuSig2 partial signs over on the refund path. |
| `created_at_unix` | u64 | Wall-clock at `init`. |
| `updated_at_unix` | u64 | Wall-clock at last phase transition. |
| `insert_push_attempts` | u8 | Bounded by `Config::insert_push_retry`. |
| `cancel_reason` | optional string | Free-text user-provided reason. Set when `status = canceled`. |
| `canceled_by_pgp_fp` | optional string | Which party initiated the cancel. |
| `htlc_refund_contract_id` | optional string | RGB contract id of the timeout-bound backup record (see §HTLC backup refund). |

**What is NOT in the row:**

- Plaintext PGP payloads — the referee receives ciphertext only.
- ZKP proofs — they are verified at `start_swap` time and not
  reconstructed; the audit log records the verification outcome with
  domain separation, which is sufficient for replay.
- MuSig2 secret nonces — they live in a process-local secret store
  on the configured backend with the same `swap_id` key but a
  separate keyspace (`referee:musig2-secret:{swap_id}` on Redis,
  `RefereeMusig2Secrets` table on DynamoDB, `referee/musig2-secret`
  subspace on FDB) and are deleted at terminal phases.
- The full state-blob — see below.

### State-blob (separate attribute)

Alongside the explicit `Transaction` fields, each row carries a
`state_blob` attribute holding the canonical JSON of
`SwapState<P>` for the row's current phase. This is what
`advance_swap` deserializes to make a transition. It is a
*derived-from-phase* implementation detail, not part of the
user-facing model — the documentation, list endpoints, and operator
tools work off the explicit fields.

The blob carries information the explicit fields don't (e.g. the
ZKP-verified ciphertexts that need to be re-pushed on insert
retry). It is opaque to anyone but the orchestrator's transition
functions.

## Status transitions

The user-facing `status` enum is a coarse projection of the
phase typestate:

```
phase           status
─────           ──────
init            pending
zkps-verified   pending
pre-checked     pending
insert-pushed   pending
aborted         pending          (transient, on the way to refunded)
invalidated     pending          (transient, on the way to refunded)
settled         settled
refunded        refunded
canceled        canceled
```

Terminal phases (`settled`, `refunded`, `canceled`) cannot transition
further. The audit log retains every prior phase chain-linked.

## Indexes (per backend)

The referee exposes `GET /v1/parties/{pgp_fp}/swaps` to list a
party's swaps. Implementation per backend:

### DynamoDB

Two GSIs on the `RefereeSwaps{-suffix}` table:

| Index | Hash | Sort | Projects |
|---|---|---|---|
| `byBob` | `bob_pgp_fp` | `created_at_unix` | `swap_id, status, phase, alice_pgp_fp, terminal, updated_at_unix` |
| `byAlice` | `alice_pgp_fp` | `created_at_unix` | `swap_id, status, phase, bob_pgp_fp, terminal, updated_at_unix` |

`list_by_party` queries both indexes and merges, deduping
swaps where the same fingerprint appears as both Bob and Alice
(legitimate self-swaps).

### Redis

Auxiliary sorted-sets keyed by fingerprint:

```
referee:by-bob:{fp}    ZADD swap_id (score = created_at_unix)
referee:by-alice:{fp}  ZADD swap_id (score = created_at_unix)
```

`list_by_party` runs `ZREVRANGEBYSCORE` on both, fetches each row by
id (`HMGET referee:swap:{id}`), and merges.

### FoundationDB

```
referee/by-bob/{fp}/{created_at:020}/{swap_id}    → ""
referee/by-alice/{fp}/{created_at:020}/{swap_id}  → ""
```

`list_by_party` does a range scan on each subspace and follows back
to the row. Zero-padded `created_at` ensures lexicographic == time
order.

## Cancellation

Each party submits an Ed25519 `cancel_pubkey_hex` at initiate
(alongside their PGP pubkey). This key is dedicated to cancel
authentication; it is independent of the PGP keypair so a wallet
that doesn't control its PGP private key (e.g. delegated to an
external PGP daemon) can still authenticate cancels.

### `POST /v1/swap/{id}/cancel`

```json
{
  "by_pgp_fp": "<40 hex>",
  "reason": "user changed mind",
  "signature_hex": "<128 hex Ed25519 sig>"
}
```

Canonical signed message:

```
"webycash-referee/cancel-swap-v1:" || swap_id || ":" || by_pgp_fp || ":" || sha256(reason)
```

Signed with the matching `cancel_pubkey_hex` registered at initiate.
The referee verifies under the appropriate party's key and rejects
with HTTP 401 on signature mismatch.

### Permission policy

| Current phase | Bob can cancel | Alice can cancel | Notes |
|---|---|---|---|
| `init`, `zkps-verified`, `pre-checked` | ✓ unilateral | ✓ unilateral | Nothing has been pushed; cancel is free. |
| `insert-pushed` | ✓ unilateral | ✗ (must wait) | Alice has already been asked to claim. Bob can withdraw the offer; Alice cannot abandon mid-claim. |
| `aborted`, `invalidated` | ✗ | ✗ | Refund path is engaged; let it run. |
| `settled`, `refunded`, `canceled` | ✗ | ✗ | Terminal. |

After `insert-pushed`, Alice's only path to bow out is to refuse
the claim and let the post-check loop exhaust retries → abort →
refund. The HTLC backup record (below) covers the case where the
referee disappears during that window.

### Side effects on cancel

1. New audit entry, phase `canceled`, payload
   `{by, reason_sha256}`.
2. Top-level row attributes updated:
   `status=canceled`, `phase=canceled`, `terminal=true`,
   `cancel_reason`, `canceled_by_pgp_fp`, `updated_at_unix`.
3. Best-effort `Invalidate` push to whichever counterparty has been
   notified of an in-flight payload (Alice if past `insert-pushed`;
   noop if pre-`insert-pushed`).
4. MuSig2 secret nonces for the swap deleted from the secret store.
5. RGB swap-tracking record marked `canceled` so the HTLC backup
   record's timeout path is short-circuited (no double-refund).

## HTLC backup refund

The referee mints a timeout-bound HTLC record on the RGB server at
initiate-time, in addition to the existing swap-tracking record.
This record exists so Alice has an indelible on-chain evidentiary
trail independent of the referee's continued availability.

### Contract shape

The record is an RGB20 unit (or a single RGB21 record) parameterised
on:

- `timeout_unix` = `created_at_unix + Config::swap_max_age_secs`
  (default 24h).
- `refund_unlock_hash` = `sha256(R_alice)` where `R_alice` is a
  32-byte secret Alice commits to at initiate (separate from the
  webcash secret; supplied via the `alice` payload's
  `htlc_refund_commitment` field).
- `referee_settle_pubkey` = the referee's Ed25519 identity pubkey.
- `bob_pgp_fp`, `alice_pgp_fp` for audit.

### Outcomes

| Swap outcome | HTLC record outcome |
|---|---|
| `settled` | Referee posts a signed `close-settle` to RGB; record archived. |
| `refunded` (MuSig2 path) | Referee posts a signed `close-refund` to RGB; record archived. |
| Referee disappears mid-`insert-pushed` | After `timeout_unix`, Alice posts `R_alice` to RGB; record releases the refund-evidentiary token to her. |
| `canceled` | Referee posts signed `close-cancel`; same archival path as settle. |

The HTLC token is **evidentiary**, not directly Bitcoin-spending.
It does not unlock Alice's ARK vtxo — that requires the MuSig2
co-signature. What it gives Alice is a public, timestamped record
that the swap aborted, which she can present to any external
arbitration / insurance / reputation mechanism. The referee's
incentive is to never get into a state where this record times out
in Alice's favour, since doing so is publicly visible.

### Wiring

- `RgbClient` gets a new method
  `mint_htlc_refund(&self, swap_id, params) -> Result<ContractId>`.
- `start_swap` calls it after the swap-tracking record mint, stores
  the returned id in `htlc_refund_contract_id`.
- `advance_swap`'s settle / refund / cancel branches each post the
  appropriate close marker.

The RGB server's existing HTLC primitive (`asset-rgb/src/htlc/`) is
the implementation; the referee is a client.

## Audit log relationship

The audit log (`AuditLog` trait) is the source of truth for
*history* — every phase transition is appended as a signed entry
with a hash chain. The `Transaction` row is the source of truth for
*current state* — what's the swap doing right now. Each says one
thing well; together they cover the operator's needs.

| Question | Source |
|---|---|
| What is this swap's current status? | `Transaction` row |
| What did this swap do at every step? | Audit log |
| What swaps has Bob participated in? | `Transaction` GSI / index |
| Has the referee been honest about phase X? | Audit log signature chain |

## Out of scope for v0.4.0

- Full bilateral cancel ceremony (both parties co-sign a cancel
  message). v0.4.0 is unilateral pre-`insert-pushed`; bilateral can
  be layered later without a schema change.
- Pagination on the history endpoint beyond a cap of 1000 results.
- Soft-deletion of canceled swaps after a retention window.
- Multi-region replication of the DynamoDB tables.
