# Push notification — webhook contract

## Why push is out of scope to implement

The referee never runs a push-notification service. Push delivery is an
**operational concern of the deployer**, who chooses a push provider
(Web Push, Firebase Cloud Messaging, Apple Push Notification, custom)
and configures the referee to call its webhook.

Reasons:

- Push providers have wildly different APIs, throttle policies, retry
  semantics, and onboarding flows. Embedding any one of them in this
  crate would tie deployments to a specific vendor.
- The cryptographic guarantees of the referee don't depend on push
  delivery: pushes are *liveness optimisations*, not safety. If push
  delivery fails entirely, the referee aborts and the abort path
  refunds Alice (HTLC timeout fallback on the wallet side, not server-
  enforced for the webcash leg per the protocol).
- Multiple deployments need different providers (corporate, public
  testnet, harmoniis, …). Decoupling at the webhook contract lets each
  deployment plug in their own.

## The contract

The referee POSTs JSON to a configured `REFEREE_PUSH_WEBHOOK_URL` with
HMAC-SHA256 authentication via the `X-Push-HMAC` header. The push
provider implements this contract; we do not.

### Request from referee → push provider

`POST {push_webhook_url}`
- Header: `Content-Type: application/json`
- Header: `X-Push-HMAC: <hex sha256-hmac of body, 32-byte key>`
- Body:

```json
{
  "swap_id": "<uuid>",
  "recipient_pgp_fp": "<40-hex>",
  "kind": "insert" | "invalidate" | "release-settle" | "release-refund",
  "payload_b64": "<base64 of the opaque payload>",
  "callback_url": "https://referee.example/v1/swap/<id>/ack"
}
```

The push provider:

1. Validates `X-Push-HMAC` against the configured shared key.
2. Looks up the device(s) registered for `recipient_pgp_fp` in its own
   registration store (out of scope for the referee).
3. Delivers the push payload to the recipient's device.
4. When the recipient wallet acks, POSTs the ack to `callback_url`.

If the push provider returns non-2xx, the referee treats the push as
failed and proceeds with retries (per `REFEREE_INSERT_PUSH_RETRY`, default 3).

### Recipient ack callback (push provider → referee)

`POST {callback_url}`

```json
{
  "kind": "insert" | "invalidate" | "release-settle" | "release-refund",
  "receipt_sig_hex": "<128-hex Ed25519 sig over canonical receipt body>"
}
```

The recipient's wallet signs the canonical receipt body
`"recipient:v1:" + kind + ":" + sha256_hex(swap_id_bytes)` with their PGP
identity key (the same key that owns `recipient_pgp_fp`).

The referee currently accepts and logs receipts but does not gate state
transitions on them in the M-5 implementation; future work (tracked in
`docs/trust-model.md`) folds invalidate-acks into the state machine so
refunds only fire after Bob provably acked the invalidation.

## Payload encoding by `kind`

### `insert`

`payload_b64` is base64 of the opaque PGP ciphertext addressed to
`recipient_pgp_fp`. The recipient wallet recovers the payload locally
with its PGP private key and calls webylib's `insert_hook` (see
`docs/hook-contract.md`).

For Webcash↔ARK swaps, the recovered payload is a webcash secret. For
future swap shapes, the payload type is determined by the type tag
inside the recipient's `insert_hook` implementation.

### `invalidate`

`payload_b64` is base64 of the public hash to invalidate (UTF-8 hex,
typically 64 ASCII characters). The recipient wallet calls webylib's
`invalidate_hook(public_hash)` to atomically `/replace` the matching
secret in its local store with a fresh self-owned one. This makes any
prior copy of the secret no longer redeemable.

### `release-settle`

`payload_b64` is base64 of a JSON blob:

```json
{
  "referee_partial_sig": "<32-byte hex MuSig2 partial-sig scalar>",
  "alice_enc_partial_sig": "<base64 of EncSig_A_to_B>"
}
```

Recipient is **Bob**. Bob's wallet:

1. Decrypts `alice_enc_partial_sig` with Bob's PGP private key, recovers
   Alice's MuSig2 partial-sig.
2. MuSig2-aggregates Alice's partial + the referee's partial.
3. Broadcasts `TX_settle` on Bitcoin ARK, claims the vtxo.

### `release-refund`

`payload_b64` is base64 of the referee's `TX_refund` MuSig2 partial-sig
scalar, hex-encoded (no encryption — Alice is the recipient).

Recipient is **Alice**. Alice's wallet:

1. Already holds her own `s_A_refund` partial-sig locally.
2. Aggregates with the referee's partial.
3. Broadcasts `TX_refund` on Bitcoin ARK, refunds the vtxo.

## HMAC key handling

The HMAC shared secret is provisioned at referee config time:
`REFEREE_PUSH_WEBHOOK_HMAC_KEY_PATH` points to a file with a
hex-encoded 32-byte key. Production deployments source the key from
KMS / secret manager and write a tmpfs file at boot.

Rotation: stop the referee, rotate the file, restart. The push provider
must rotate the same key in lockstep — coordinate via a shared release
channel.

## Idempotency requirements on the push provider

The referee's retry policy (default 3 attempts on insert) means the
push provider may receive the same `(swap_id, kind, payload_b64)` tuple
multiple times. The push provider MUST be idempotent: detect duplicates
and avoid sending the same payload to the recipient device twice.

Recommended dedup key: `(swap_id, kind, sha256_hex(payload_b64))`.

## Failure semantics

| Failure | Referee response |
|---|---|
| Push provider returns 4xx (auth / malformed) | Logs error, surfaces 502 to caller of `/v1/swap/initiate`, swap stays at `insert-pushed` until manual intervention or timeout |
| Push provider returns 5xx | Treated as transient; retried with exponential backoff |
| Push provider unreachable | Treated as transient |
| Push provider acks 200 but recipient never acks via callback | Pushes count as dispatched; orchestrator proceeds with post-check; if post-check fails, retries; if retries exhausted, abort path |
| Recipient's device unreachable for the entire swap window | Push provider's problem; refund path engages on the referee side |
