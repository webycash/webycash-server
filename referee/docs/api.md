# Referee — HTTP API specification

All endpoints live under `/v1/`. Every response is `application/json`.

## `GET /v1/pubkey`

Returns the referee's Ed25519 identity pubkey + MuSig2 pubshare. Pinned
by wallets at first contact; rotation requires re-pinning.

**Response 200:**

```json
{
  "ed25519_pubkey_hex": "<64-hex>",
  "musig2_pubshare_hex": "<66-hex compressed secp256k1>",
  "referee_version": "0.3.1"
}
```

## `POST /v1/swap/initiate`

Begin a swap. The body carries both parties' encrypted payloads + ZKPs +
Alice's MuSig2 nonce commitments. The referee then runs the entire
typestate flow synchronously (verify ZKPs → pre-check → insert push →
post-check → release settle / abort → refund) and responds with the
terminal outcome.

**Request body:**

```json
{
  "parties": {
    "bob_pgp_fp": "<40-hex>",
    "bob_pgp_pubkey_hex": "<hex full PGP pubkey>",
    "alice_pgp_fp": "<40-hex>",
    "alice_pgp_pubkey_hex": "<hex full PGP pubkey>",
    "alice_musig2_pubkey": "<66-hex compressed secp256k1>",
    "bob_cancel_pubkey_hex": "<64-hex 32-byte Ed25519>",
    "alice_cancel_pubkey_hex": "<64-hex 32-byte Ed25519>"
  },
  "bob": {
    "h_b": "<64-hex sha256(S_B)>",
    "enc_secret_for_alice": { "bytes": [/* PGP ciphertext bytes */] },
    "zkp_payload": {
      "proof": [/* Groth16 proof bytes */],
      "public_inputs": [[/* h_b bytes */], …]
    }
  },
  "alice": {
    "vtxo": "<64-hex outpoint hash>",
    "tx_settle_hash": "<64-hex>",
    "tx_refund_hash": "<64-hex>",
    "enc_partial_sig_for_bob": { "bytes": [/* PGP ciphertext bytes */] },
    "zkp_signature": {
      "proof": [/* Groth16 proof bytes */],
      "public_inputs": [[/* tx_settle_hash bytes */], …]
    }
  },
  "alice_nonces": {
    "settle_nonce_pub": "<132-hex 66-byte MuSig2 pub-nonce>",
    "refund_nonce_pub": "<132-hex 66-byte MuSig2 pub-nonce>"
  }
}
```

**Response 200:**

```json
{ "swap_id": "<uuid>", "phase": "insert-pushed" }
```

`/v1/swap/initiate` runs the synchronous portion of the swap to
completion: `init → zkps-verified → pre-checked → insert-pushed`.
Returns when the row is persisted in `insert-pushed`. Subsequent
post-check transitions (`settled` / `aborted → invalidated →
refunded`) happen via `POST /v1/swap/{id}/advance`, which a Lambda
scheduler invokes on a cadence; each call runs ONE transition. This
keeps every HTTP request short and Lambda-friendly.

Clients learn the terminal phase by polling
`POST /v1/swap/{id}/poll` (see below), reading the full signed
history at `GET /v1/swap/{id}/audit`, or listing
`GET /v1/parties/{pgp_fp}/swaps`.

**Error responses (synchronous — caught before the spawn):**

| Status | When |
|---|---|
| 400 Bad Request | Malformed JSON, missing fields, self-contradictory inputs |
| 500 Internal Server Error | Store unreachable (placeholder-row write failed) |

Errors that surface only inside the spawned orchestration (ZKP
rejection, pre-check `H_B` already spent, push-provider 5xx) are
recorded in the audit log as the failed phase entry; the swap row's
`phase` reflects the last successful transition. Clients detect these
by polling and observing that the swap has not progressed past
`init` / `pre-checked` after a reasonable interval, then reading
`/v1/swap/{id}/audit` for the failure detail.

Body shape on every error:

```json
{ "error": "<human-readable>", "kind": "<RefereeError variant Debug>" }
```

## `GET /v1/swap/{id}/audit`

Read the full signed audit log for a swap. Public — no authentication.
Used by auditors to verify the referee's behaviour against the protocol.

**Response 200:** array of `AuditEntry` (see `audit.rs`):

```json
[
  {
    "swap_id": "<uuid>",
    "phase": "init",
    "ts_unix": 1714003200,
    "prior_tip": "",
    "phase_payload": { /* phase-specific */ },
    "signature": "<128-hex Ed25519 sig>"
  },
  …
]
```

Each entry's `prior_tip` MUST equal `sha256(prior_entry.canonical_body())`
hex (where canonical body = the entry's JSON without the signature
field). Auditors verify the chain by walking forward from `prior_tip == ""`.

## `POST /v1/swap/{id}/advance`

Run one state-machine transition on a swap. Idempotent: a Lambda
scheduler invokes this on a cadence; once the swap is terminal,
further calls return the terminal phase without dispatching duplicate
side-effects.

**Request body:** empty `{}` (a future field `as_of` for drift
detection is reserved).

**Response 200:**

```json
{ "swap_id": "<uuid>", "phase": "<current phase>", "terminal": true|false }
```

## `POST /v1/swap/{id}/cancel`

Either party cancels a stuck swap. The signature authenticates the
request as coming from the named party.

**Request body:**

```json
{
  "by_pgp_fp": "<40-hex>",
  "reason": "<free-text reason>",
  "signature_hex": "<128-hex Ed25519 sig>"
}
```

The signature is over the canonical message:

```
"webycash-referee/cancel-swap-v1:" || swap_id || ":" || by_pgp_fp || ":" || sha256_hex(reason)
```

…signed with the party's `cancel_pubkey_hex` registered at initiate.

**Permission policy** (see `docs/transaction-model.md` §Cancellation):

| Current phase | Bob | Alice |
|---|---|---|
| `init`, `zkps-verified`, `pre-checked` | ✓ unilateral | ✓ unilateral |
| `insert-pushed` | ✓ unilateral | ✗ — wait for refund |
| any other phase | ✗ | ✗ |

**Response 200:**

```json
{ "swap_id": "<uuid>", "phase": "canceled", "terminal": true }
```

| Status | When |
|---|---|
| 400 | `by_pgp_fp` doesn't match either party of this swap, or unknown swap_id |
| 401 (Crypto) | Cancel signature failed verification |
| 409 | Phase is terminal or post-`insert-pushed` for Alice |

## `POST /v1/swap/{id}/poll`

Read the full top-level transaction shape. Wallets poll this when
push delivery is delayed or to confirm terminal status.

**Response 200:**

```json
{
  "swap_id": "<uuid>",
  "status": "pending|settled|refunded|canceled",
  "phase": "<typestate phase>",
  "terminal": true|false,
  "bob_pgp_fp": "<40-hex>",
  "alice_pgp_fp": "<40-hex>",
  "created_at_unix": 1714003200,
  "updated_at_unix": 1714003205,
  "insert_push_attempts": 0,
  "cancel_reason": null,
  "canceled_by_pgp_fp": null,
  "htlc_refund_contract_id": "rgb-htlc-…"
}
```

`phase` is one of: `init`, `zkps-verified`, `pre-checked`,
`insert-pushed`, `settled`, `aborted`, `invalidated`, `refunded`,
`canceled`.

## `GET /v1/parties/{pgp_fp}/swaps`

Reverse-chronological history of every swap the fingerprint
participated in (as Bob, as Alice, or as both — self-swaps).
Capped at 1000 results.

**Response 200:** array of `TransactionSummary`:

```json
[
  {
    "swap_id": "<uuid>",
    "status": "pending|settled|refunded|canceled",
    "phase": "<typestate phase>",
    "terminal": true|false,
    "bob_pgp_fp": "<40-hex>",
    "alice_pgp_fp": "<40-hex>",
    "role": "bob|alice|both",
    "created_at_unix": 1714003200,
    "updated_at_unix": 1714003205
  }
]
```

Backed by DynamoDB GSIs (`byBob`, `byAlice`), Redis sorted-sets, or
FoundationDB index subspaces — all single-roundtrip.

## `POST /v1/swap/{id}/ack`

Recipient wallet ack callback. The push provider POSTs here when the
wallet has handled an `insert_hook` / `invalidate_hook` / `release_settle`
/ `release_refund` push.

**Request body:**

```json
{
  "kind": "insert" | "invalidate" | "release-settle" | "release-refund",
  "receipt_sig_hex": "<128-hex Ed25519 sig over canonical receipt>"
}
```

The receipt-sig allows the audit log to record that the wallet provably
handled the push. In the current implementation acks are accepted with
status 200 but not yet folded into the typestate — extending the
typestate to require receipt-acks for the abort path is tracked in
`docs/trust-model.md` under "future work".

## Canonical-message format for signatures

Every Ed25519 signature the referee emits (audit log entries, ack
receipts when present) is computed as:

```text
canonical = "referee:v1:" + tag + ":" + sha256_hex(body_bytes)
sig = Ed25519_sign(referee_secret_key, canonical)
```

`tag` is one of the variants in `crate::sign::Tag`. See
`docs/musig2-ceremony.md` for tag-vs-phase correspondence.

## Idempotency

`/v1/swap/initiate` is **not** idempotent — every call begins a fresh
swap. Wallets that crash mid-call must drive recovery via
`/v1/swap/{id}/poll` after re-establishing identity.

Push retries (driven by the orchestrator) are idempotent on the
recipient wallet's side: the wallet matches by `swap_id` + `kind` +
payload hash and discards duplicates.
