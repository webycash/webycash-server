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
  "referee_version": "0.2.3"
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
    "alice_musig2_pubkey": "<66-hex compressed secp256k1>"
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

**Response 200 (accepted):**

```json
{ "swap_id": "<uuid>", "status": "accepted" }
```

`/v1/swap/initiate` returns immediately after persisting the swap and
spawning the orchestration in the background. The orchestrator
progresses through `init → zkps-verified → pre-checked → insert-pushed
→ settled|aborted → invalidated → refunded`; clients learn the terminal
phase by polling `GET /v1/swap/{id}/poll` (see below) or by reading the
full signed history at `GET /v1/swap/{id}/audit`.

This shape exists because the orchestration includes the post-check
loop on the webcash leg, which can take seconds to many minutes
depending on the configured `REFEREE_RETRY_BACKOFF_MS` × retry budget;
holding the HTTP request open that long would tie up server resources
and risk client-side timeouts.

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

## `POST /v1/swap/{id}/poll`

Read the current phase + last update time. Wallets poll this when push
delivery is delayed.

**Response 200:**

```json
{ "phase": "insert-pushed", "updated_at_unix": 1714003205 }
```

`phase` is one of: `accepted` (transient — placeholder written before the
spawned task takes over), `init`, `zkps-verified`, `pre-checked`,
`insert-pushed`, `settled`, `aborted`, `invalidated`, `refunded`.

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
