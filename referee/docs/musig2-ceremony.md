# MuSig2 ceremony — exact protocol the referee runs

## Why two sessions per swap

A single 2-of-2 (Alice, referee) vtxo is signed in **two different
contexts** within one swap:

- `TX_settle` — releases the vtxo to Bob's pubkey. Authorised on the
  success path.
- `TX_refund` — returns the vtxo to Alice's pubkey. Authorised on the
  abort path.

These two transactions are different message digests; MuSig2 (BIP327)
requires a fresh nonce-pair per message. Reusing nonces across two
different messages with the same key allows recovery of the secret key.
We therefore run **two independent MuSig2 sessions per swap**, one per
transaction.

## Round 1 — nonce exchange (at swap-init)

Both Alice and the referee generate `(secnonce, pubnonce)` pairs from a
CSPRNG. The pubnonces are exchanged once and committed to the audit log
before any partial-sig is produced.

```
Alice                                  Referee
─────                                  ───────
secnonce_S_A, pubnonce_S_A    secnonce_S_R, pubnonce_S_R   (settle session)
secnonce_R_A, pubnonce_R_A    secnonce_R_R, pubnonce_R_R   (refund session)

       pubnonce_S_A ────────────────▶
       pubnonce_R_A ────────────────▶
                          ◀──── pubnonce_S_R     (returned in /v1/swap/initiate response /
                          ◀──── pubnonce_R_R      preview embedded into the audit-log "init" entry)
```

The referee's `MockSigner::begin_session` (and the production signer)
produces a pubnonce + stores the secnonce keyed by `(SwapId, Session)`.
Secret nonces never leave the signer's process memory.

## Round 2 — partial signing (at terminal phase)

Each side computes its partial-sig for the relevant message using:

- their own `secnonce` for that session
- the OTHER side's `pubnonce` (pre-committed)
- the message digest (`tx_settle_hash` or `tx_refund_hash`)

### Alice's partial-sigs (produced wallet-side)

Alice produces:

- `s_A_settle = MuSig2_partial_sign(secnonce_S_A, secret_A, msg = tx_settle_hash, both_pubnonces, both_pubshares)`
- `s_A_refund = MuSig2_partial_sign(secnonce_R_A, secret_A, msg = tx_refund_hash, both_pubnonces, both_pubshares)`

She **encrypts `s_A_settle` to Bob's PGP pubkey** and submits the
ciphertext + a Groth16 ZKP that the ciphertext decrypts to a valid
partial-sig (see `docs/zkp-circuits.md` Alice's circuit). She **keeps
`s_A_refund` strictly local** — it is never given to anyone.

### Referee's partial-sigs (produced server-side at terminal phase)

On the **success path**, the referee produces `s_R_settle` and pushes
`(s_R_settle, EncSig_A_to_B)` to Bob. Bob decrypts to recover `s_A_settle`,
aggregates `s_A_settle + s_R_settle` (BIP327), broadcasts `TX_settle`.

On the **abort path**, the referee produces `s_R_refund` and pushes it
**cleartext to Alice** (she's the recipient). Alice has her local
`s_A_refund` and aggregates them; broadcasts `TX_refund`.

## Cryptographic invariants (enforced)

Restated from `referee-zkp-based-swap.md §5`:

1. The referee NEVER holds Alice's `s_A_settle` in cleartext. It is
   encrypted-to-Bob throughout.
2. The referee NEVER holds Alice's `s_A_refund` at all. Alice keeps it
   local; the referee only contributes its own `s_R_refund`.
3. The referee's two secret nonces (`secnonce_S_R`, `secnonce_R_R`) are
   per-session and discarded after partial-signing. The trait
   `Musig2Signer` exposes `discard_session(swap_id, session)` for
   explicit disposal on terminal paths so neither nonce lingers in
   memory after the swap ends.
4. Nonce-pair freshness: each call to `Musig2Signer::begin_session`
   generates fresh nonces. Reuse is prevented by the implementation
   (the `MockSigner` rejects a second `begin_session` on the same
   `(SwapId, Session)`, and the production signer does the same).

## Why the referee can't recover Alice's secret

For nonce-reuse / Wagner-style key recovery against MuSig2, an attacker
would need:

- two of Alice's partial-sigs, in cleartext
- with their corresponding pubnonces visible
- where the underlying secnonces overlap

In our protocol:

- The referee never sees `s_A_settle` cleartext (encrypted-to-Bob).
- The referee never sees `s_A_refund` at all.
- Even Bob (who decrypts `s_A_settle`) never sees `s_A_refund`, and
  vice versa for Alice on the abort path.

The only party that ever holds **both** of Alice's partial-sigs is
Alice. The pubnonces are public (audit log) but pubnonces alone are
useless for recovery without the partial-sigs they bind to.

## What sessions look like in code

```rust
let id = SwapId::fresh();
let n_settle = signer.begin_session(&id, Session::Settle).await?;  // returns pubnonce
let n_refund = signer.begin_session(&id, Session::Refund).await?;

// … swap lifecycle proceeds …

// Success path:
let referee_partial_settle = signer
    .partial_sign(&id, Session::Settle, tx_settle_hash, &alice_pubshare, &alice_pubnonce_settle)
    .await?;
signer.discard_session(&id, Session::Refund).await?;  // never used; explicitly drop

// Abort path:
let referee_partial_refund = signer
    .partial_sign(&id, Session::Refund, tx_refund_hash, &alice_pubshare, &alice_pubnonce_refund)
    .await?;
signer.discard_session(&id, Session::Settle).await?;  // never used; explicitly drop
```

## Key generation + storage

The referee's MuSig2 secret key is generated once, persisted as a
sealed file (`REFEREE_MUSIG2_KEY_PATH` — same operational discipline as
`REFEREE_IDENTITY_KEY_PATH`). Production deployments load it from KMS
at boot. See `docs/deployment.md`.

## Tag-to-phase mapping (audit log)

| Phase | Audit-log tag | What's signed |
|---|---|---|
| `init` | `initiate-ack` | Both parties' fingerprints + commitment hashes |
| `zkps-verified` | `zkps-verified` | Boolean outcomes + ZKP proof hashes |
| `pre-checked` | `pre-checked` | Webcash health-check response hash |
| `insert-pushed` | `insert-pushed` | Attempt counter + push-request hash |
| `settled` | `settled` | Final partial-sig + encrypted-to-Bob blob hashes |
| `aborted` | `aborted` | Attempt count + abort timestamp |
| `invalidated` | `invalidated` | Bob's invalidate-hook ack receipt |
| `refunded` | `refunded` | Refund partial-sig hash |
