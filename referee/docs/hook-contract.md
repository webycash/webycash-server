# Hook contract for wallet implementors (extro-node)

This document specifies the `insert_hook` and `invalidate_hook` functions
that **wallet implementors** (extro-node, harmoniis-wallet, custom PWAs)
must expose so the referee can drive cross-rail swaps end-to-end.

The hooks live in the **wallet implementor's project**, not in this
crate, not in webylib. They are called by the wallet implementor's
service-worker / push handler when a referee-driven push arrives. They
operate on the wallet's local key material — which webylib never sees.

## `insert_hook(pgp_pub, encrypted_payload, kind)`

### Signature (suggested)

```rust
pub trait WalletImplementor {
    /// Called when the wallet receives an `insert` push from the referee.
    ///
    /// - `pgp_pub`: the recipient's PGP fingerprint that the payload is
    ///   addressed to (the wallet may host multiple identities).
    /// - `encrypted_payload`: opaque PGP ciphertext addressed to that
    ///   pubkey. Decrypt locally; webylib never sees cleartext.
    /// - `kind`: what type the cleartext is (Webcash, Voucher, …).
    fn insert_hook(
        &self,
        pgp_pub: &str,
        encrypted_payload: &[u8],
        kind: BearerKind,
    ) -> Result<InsertOutcome, ImplementorError>;
}

pub enum BearerKind { Webcash, Voucher }

pub enum InsertOutcome {
    /// The cleartext was decrypted, validated, and a /replace was
    /// successfully submitted to the bearer-cash server.
    Replaced,
    /// The cleartext decrypted but the matching public-hash was already
    /// spent by the time we tried /replace. (Race lost.)
    AlreadySpent,
    /// The decrypted cleartext didn't pass the wallet's structural checks
    /// (length, hash matches, etc.).
    InvalidPayload(String),
}
```

### Required behaviour

1. **Decrypt locally**. Use the wallet's PGP private key for `pgp_pub`.
   Do NOT log cleartext. Do NOT send cleartext over IPC unless the IPC
   peer is itself trusted (e.g. service-worker → in-page WASM).
2. **Submit `/replace` to the matching bearer-cash server**.
   - For `BearerKind::Webcash`: webcash.org's `/api/v1/replace`.
   - For `BearerKind::Voucher`: the relevant voucher server.
   The output secret should be a fresh wallet-owned secret derived from
   the wallet's HD chain (see harmoniis-wallet's `keychain.rs` for the
   BIP32 family conventions).
3. **Insert the new owned secret** into the wallet's local store
   (SQLite / IndexedDB / extrolib-store).
4. **Ack the push** by POSTing back to the referee's callback URL with
   a signed receipt.

### Failure semantics

If decrypt fails: ack with `InvalidPayload` and log. The referee will
retry (up to its configured limit, default 3) before aborting.

If `/replace` fails because the public-hash is already spent: ack with
`AlreadySpent`. This is a race-loss; the referee's post-check will
observe the same and proceed to settle (releasing the vtxo to Bob, who
front-ran).

## `invalidate_hook(public_hash)`

### Signature (suggested)

```rust
pub trait WalletImplementor {
    /// Called when the wallet receives an `invalidate` push from the
    /// referee. Bob's wallet must atomically replace the matching secret
    /// to a fresh self-owned secret, neutralising any cleartext that
    /// may have leaked.
    fn invalidate_hook(
        &self,
        public_hash: &str,
    ) -> Result<InvalidateOutcome, ImplementorError>;
}

pub enum InvalidateOutcome {
    /// Successfully replaced; old cleartext is now worthless.
    Invalidated,
    /// No matching secret in the wallet (the wallet doesn't own this
    /// hash). Returns immediately; no `/replace` issued.
    NotOurs,
    /// Already invalidated (idempotent path — the wallet has seen this
    /// invalidate before).
    AlreadyInvalidated,
}
```

### Required behaviour

1. **Look up `public_hash` in the wallet's local store**.
2. If not found, return `NotOurs`. (The referee may have routed the
   push to the wrong fingerprint, or this is a stale invalidate from a
   prior swap.)
3. If found, **submit `/replace`** swapping the original secret for a
   fresh wallet-owned secret on the matching bearer-cash server. Both
   inputs and outputs share the wallet's namespace.
4. Mark the original secret as invalidated in the local store (so a
   replay of the same invalidate is a no-op).
5. Ack with `Invalidated`.

### Why this exists

On the abort path, the referee believes the cleartext webcash secret
*may* have leaked (Alice may have decrypted it before her `/replace`
failed). `invalidate_hook` lets Bob neutralise that leak by `/replace`-
ing himself before any racing party can.

The wallet implementor's service-worker MUST handle this hook even if
the user is offline or the device is locked: queue it, persist the
queue durably, replay on next reachable opportunity. If the queue
exceeds 24h, escalate to the user (Bob is at risk if the secret leaked
and Alice front-runs his invalidation).

## Authentication

The push provider's webhook (which delivers the push to the wallet)
authenticates with the wallet via push-provider-specific mechanism (Web
Push: VAPID; FCM: device tokens; APNs: tokens). That's the push
provider's concern.

The wallet authenticates the **referee** by pinning the referee's
Ed25519 pubkey at first-pair time and verifying every signed message
the referee emits (audit log entries, ack-receipt expectations).
Wallet implementors fetch the pinned pubkey from
`https://referee.example/v1/pubkey` over HTTPS and store it locally.

## Idempotency

Both hooks MUST be idempotent. The push provider may deliver duplicates
(referee retries, network re-tries, push provider re-tries). The wallet
implementor de-duplicates by `(swap_id, kind, payload_hash)` and
short-circuits on duplicate.

## Tested by extro-node

This crate ships only the *server-side* counterpart and mocks. The
contract above is for **wallet implementors** (extro-node primarily;
also harmoniis-wallet and any third-party PWA). Tests of the hook
implementations live in those projects.

The integration test in `referee/tests/orchestrator_e2e.rs` exercises
the orchestrator's *push dispatch* side of the contract using
`MockPush`. The full wallet-side handling (PGP decrypt, `/replace`,
local store updates) is exercised in extro-node's test suite.
