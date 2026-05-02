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

### Invariant 1: referee never holds Alice's `TX_settle` partial-sig in cleartext

Alice encrypts her `s_A_settle` to Bob's PGP pubkey before submitting it
to `/v1/swap/initiate`. The Groth16 ZKP proves the ciphertext is honest
without revealing `s_A_settle`. The referee verifies the proof, stores
the ciphertext in [`PgpEncrypted<AliceTxSettlePartialSig>`] (a newtype
that doesn't expose decryption methods), and forwards it as opaque
bytes inside the `release-settle` push.

If the referee tried to broadcast `TX_settle` itself, it would need
Alice's cleartext `s_A_settle` to MuSig2-aggregate with its own
partial-sig. It does not have it. End of story.

### Invariant 2: referee never holds Alice's `TX_refund` partial-sig

Alice keeps `s_A_refund` strictly local. It is never submitted to the
referee in any form. The referee's `release-refund` push contains only
the referee's own partial-sig; Alice combines it with her local
partial-sig to produce the final aggregate.

A malicious referee could *refuse* to send `release-refund`. In that
case Alice's vtxo stays in the 2-of-2; she can wait for the on-chain
HTLC timeout (which we engineer Alice's vtxo to have natively per
ARK's script paths) and refund unilaterally without referee
cooperation.

### Invariant 3: referee never holds `S_B` in cleartext

Bob encrypts `S_B` to Alice's PGP pubkey before submitting it to
`/v1/swap/initiate`. The Groth16 ZKP proves the ciphertext decrypts to
a 32-byte value with `sha256(S) = H_B`. The referee verifies the
proof, stores the ciphertext in [`PgpEncrypted<WebcashSecret>`], and
forwards it as opaque bytes inside the `insert-hook` push.

A malicious referee could not `/replace` the webcash itself even if it
wanted to: it has no cleartext.

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
| Steal Bob's webcash | Cleartext `S_B` never reaches the referee |
| Steal Alice's vtxo | The 2-of-2 + native HTLC ensures she always has the unilateral refund path |
| Forge audit entries | Signatures are Ed25519; auditors verify against the pinned pubkey |
| Run a swap with parameters different from what was committed | The audit log's `init` entry commits to the parameters; any divergence breaks the chain |
| Reuse a MuSig2 nonce | `Musig2Signer::begin_session` rejects a second begin on the same `(SwapId, Session)` pair |

## Operational threats

These are NOT cryptographic but matter operationally:

- **Key compromise**: if the referee's Ed25519 identity key leaks, an
  attacker can forge audit entries from that point onward. Mitigated by
  HSM-backed signing in production (key never leaves the HSM).
- **Postgres compromise**: the referee's swap-state store does not
  contain secret material (cleartexts, partial-sigs, secret nonces).
  Compromise reveals which swaps exist + their public parameters; not a
  fund-loss vector.
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
