# Referee — formal trust model

## What we claim

The referee binary, deployed honestly, **cannot steal funds**. The worst
a malicious referee can do is *delay* a swap until either side's
timeout-refund engages.

The referee, deployed dishonestly, **leaves a public, signed audit
trail** of its misbehaviour. Every action is committed to a chain of
Ed25519-signed messages plus a public RGB21 record on our RGB server.
Auditors can re-verify the chain and the embedded ZKPs after the fact.

## Adversary model

| Adversary | What they want | What they can do |
|---|---|---|
| Bob | Keep his webcash AND get Alice's vtxo | Front-run Alice's `/replace` on webcash.org during the race window (§4.5 of `referee-zkp-based-swap.md`); refuse to ack `invalidate_hook`; broadcast `TX_settle` with a stale partial-sig |
| Alice | Keep her vtxo AND get Bob's webcash | Refuse to acknowledge the swap after `/v1/swap/initiate`; refuse to broadcast `TX_settle` after Bob releases the encrypted partial-sig |
| Malicious referee | Delay; bias outcomes; censor | Refuse to dispatch pushes; sign nonsense audit entries; refuse to release partial-sigs |
| Compromised push provider | Deliver pushes to wrong recipients | The push HMAC is keyed; a compromised provider can deny service but cannot forge ack-receipts |
| Compromised webcash.org operator | Censor `/health_check` lookups | The referee treats webcash.org responses as ground truth; a compromised webcash.org could lie about `H_B`'s status, but the referee's audit log records what it observed at each timestamp |

## Why the referee can't steal — invariant by invariant

### Invariant 1: Alice's `TX_settle` partial-sig is encrypted to Bob

Alice's `s_A_settle` is encrypted to Bob's PGP pubkey before being
submitted to `/v1/swap/initiate`. The referee receives ciphertext only;
the recipient pubkey is Bob's. The Groth16 ZKP proves the ciphertext is
a well-formed encryption of a valid MuSig2 partial-signature against
the public commitments in the audit chain. The referee stores the
bytes in [`PgpEncrypted<AliceTxSettlePartialSig>`] and forwards them
unmodified inside the `release-settle` push.

For the referee to broadcast `TX_settle` itself it would need the
plaintext `s_A_settle` to MuSig2-aggregate with its own partial-sig.
The plaintext is addressed to Bob, not the referee.

### Invariant 2: `TX_refund` partial-sig stays local to Alice

`s_A_refund` is never submitted to the referee in any form. The
`release-refund` push the referee dispatches contains only the
referee's own partial-sig; Alice aggregates it with her local
partial-sig to produce the final signature.

A malicious referee can *refuse* to send `release-refund`. Alice's
vtxo then stays in the 2-of-2; she waits for the on-chain HTLC
timeout (engineered into the vtxo's ARK script paths) and refunds
unilaterally without referee cooperation.

### Invariant 3: `S_B` is encrypted to Alice

Bob's webcash secret `S_B` is encrypted to Alice's PGP pubkey before
being submitted to `/v1/swap/initiate`. The referee receives
ciphertext only. The Groth16 ZKP proves the ciphertext encrypts a
32-byte value `S` with `sha256(S) = H_B` (the public hash that
appears in the audit chain). The referee stores the bytes in
[`PgpEncrypted<WebcashSecret>`] and forwards them unmodified inside
the `insert-hook` push.

A malicious referee cannot `/replace` the webcash on its own behalf:
the plaintext is addressed to Alice.

### Invariant 4: every action is publicly auditable

Every phase transition emits a signed audit-log entry. Each entry's
`prior_tip` commits to the previous entry's `tip_hash`, so any
post-hoc tampering breaks the chain. The audit log is served read-only
at `/v1/swap/{id}/audit` — no authentication required.

Auditors verify by:

1. Walking the chain forward from the first `init` entry.
2. Re-running each Ed25519 signature verification against the pinned
   pubkey from `/v1/pubkey`.
3. Re-running each Groth16 verification against the bundled verifying
   keys.
4. Cross-checking the embedded webcash.org responses against an
   independent fresh `/health_check` (the responses are timestamped, so
   discrepancies surface time-of-check shenanigans).

Any failure in these checks is grounds for the referee operator's
reputational ouster.

## What the referee CAN do (honestly)

| Action | Cost |
|---|---|
| Accept or reject a `/v1/swap/initiate` request | Trivial — Alice and Bob retry with a different referee if rejected |
| Choose when to fire pushes | Liveness: delays the swap; safety: zero |
| Decide retry vs abort thresholds | Configured per-deployment; documented in `docs/deployment.md` |
| Refuse to release `release-settle` | Bob waits for HTLC timeout, refunds via the unilateral path |
| Refuse to release `release-refund` | Alice waits for HTLC timeout (her vtxo's native script path), refunds unilaterally |

## What the referee CANNOT do

| Attempted action | Why it fails |
|---|---|
| Steal Bob's webcash | `S_B` is addressed to Alice's PGP pubkey, not the referee's |
| Steal Alice's vtxo | The 2-of-2 + native HTLC ensures Alice always has the unilateral refund path |
| Forge audit entries | Signatures are Ed25519; auditors verify against the pinned pubkey |
| Run a swap with parameters different from what was committed | The audit log's `init` entry commits to the parameters; any divergence breaks the chain |
| Reuse a MuSig2 nonce | `Musig2Signer::begin_session` rejects a second begin on the same `(SwapId, Session)` pair |

## Operational threats

These are NOT cryptographic but matter operationally:

- **Key compromise**: if the referee's Ed25519 identity key leaks, an
  attacker can forge audit entries from that point onward. Mitigated by
  HSM-backed signing in production (key never leaves the HSM).
- **Postgres compromise**: the referee's swap-state store contains
  ciphertext blobs and public parameters only — no plaintext payloads,
  no partial-sigs, no secret nonces (those live in the `UNLOGGED`
  table that is wiped on crash). Compromise reveals which swaps exist
  and their public parameters; not a fund-loss vector.
- **Push provider compromise**: as above — denial of service, not
  fund theft. Mitigated by HMAC + recipient-ack signatures.
- **Software supply-chain**: the referee binary is reproducibly built
  and the binary hash is published with each release. Operators must
  pin specific hashes; auto-updates are explicitly NOT supported.

## Future work — slashing bond

The current trust model is *reputational*: a malicious referee can be
caught (audit log) but cannot be financially penalised. A future
upgrade adds a slashing bond — the referee posts collateral on Bitcoin
or RGB; auditors can submit fraud proofs that slash the bond and
distribute proceeds to wronged parties. Out of scope for this
milestone; tracked in the project plan.

## Future work — receipt-bound state machine

Currently, the orchestrator dispatches pushes and proceeds without
strictly requiring the recipient ack to arrive. A future upgrade folds
recipient acks into the typestate so the abort-path's `Invalidated`
phase requires Bob's signed receipt before `Refunded` is reachable. This
removes one class of disputes ("the referee released refund without
me invalidating"). Tracked alongside the slashing bond.
