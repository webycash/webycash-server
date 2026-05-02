# Cross-rail swaps on the webycash family

This document specifies the three cross-rail swap flows we ship. The phrase
*"atomic swap"* is **not used** anywhere in this family — none of these flows is
cryptographically atomic in the strict sense, and conflating them under one umbrella
hides the trust and race assumptions specific to each. Each flow has its own name and
its own §:

- **Referee-ZKP-based swap** (Webcash ↔ Bitcoin ARK) — §4.
- **HTLC swap** (RGB ↔ Bitcoin ARK) — §7.1.
- **HTLC + bearer-race swap** (RGB ↔ Webcash, RGB ↔ Voucher) — §7.2 / §7.3.

## §0. The asymmetry that drives everything

> Webcash and Vouchers are bearer cash: a swap **transfers the secret**.
> Bitcoin ARK is signature-controlled: a swap **transfers a signature**.
> RGB on our server is programmable state: a swap **transitions state under AluVM**.

The whole protocol family follows from that asymmetry. Each rail has its own primitive,
its own trust model, and its own settlement property — cryptographic for some pairs,
race-bounded for others. We never paper over the differences — we engineer with them
explicitly.

A consequence we accept up front: the only swap flow that needs an external **referee**
service is **Webcash ↔ Bitcoin ARK**, because that pair has one bearer-cash leg
and one signature leg with no shared cryptographic primitive. Every other cross-rail
flow either uses HTLC on both sides (RGB ↔ ARK — fully cryptographic) or HTLC on the
RGB side plus a race-with-pre-armed-wallet on the bearer-cash side (RGB ↔ Webcash,
RGB ↔ Voucher), neither of which needs an external mediator.

**Webcash ↔ Voucher swaps are explicitly out of scope** (§6) — vouchers are bought
directly from the voucher issuer, so a cross-rail swap between them serves no purpose.

## §1. Naming convention (context-dependent)

These names are FINAL across all webycash-family code and docs. Every term used
elsewhere is a precision break.

| Term | Meaning | Where used |
|---|---|---|
| `RgbFungible`, `RgbCollectible`, `RGB20`, `RGB21` | RGB ecosystem's own names for fungible / non-fungible records and schemata | Library code (`asset-rgb`, `wallet-rgb`, server APIs) |
| `License`, `Royalties License`, `Perpetual License` | Harmoniis's domain term for their RGB21-issued contracts. A Harmoniis convention layered on top of RGB21 — never appears in webylib library code. | Harmoniis-domain docs only |
| `referee` | Our service for **Webcash ↔ Bitcoin ARK** swaps. Holds no custody. Co-signs the ARK 2-of-2 vtxo and orchestrates the encrypted-payload exchange. | This doc, `referee` crate |
| `extronet` | Decentralized contract publishing and discovery network. Out of scope to build; webylib ships a thin client. | This doc, future `extronet-client` crate |
| `insert_hook(pgp_pub, encrypted_payload, type)` | Webylib function the referee remote-calls (via push) on a recipient's PWA. Webylib never sees cleartext; the wallet implementor's PGP private key decrypts locally. | Webylib API |
| `invalidate_hook(public_hash)` | Webylib function the referee remote-calls on the *original holder* during abort: the holder's wallet `/replaces` the matching secret to a fresh self-owned secret, neutralising any cleartext that may have leaked. | Webylib API |
| **Banned** | `NFT`, `bond`, `arbiter`, `matchmaker`, `witness server`. Never use these. | — |

## §2. Trust model

| Layer | What | Who | Threat |
|---|---|---|---|
| L1 — User keys | HD master, derived secrets, PGP private key, every signature | User-controlled exclusively | Compromise = total wallet loss; user's responsibility |
| L2 — Trusted code | Webylib, AluVM bytecode, `webycash-server-rgb` ReplaceHook validators | Reproducible builds, deterministic transitions, public source | Audit-driven trust; supply-chain integrity |
| L3 — Trusted services | `webycash-server-rgb`, `referee` binary | We operate; transparent; signed audit logs; bounded action surface | Honest-but-curious; can be slow but *cannot steal* |
| L4 — External rails | webcash.org, voucher servers, Bitcoin ARK ASP, push notification provider | Third parties | Honest-but-fixed; protocol must not depend on them changing |
| L5 — Counterparty | The other party in any swap | Adversarial | We assume defection at the worst possible moment |

The referee sits in L3. Its non-custodial property is enforced *cryptographically*
by the encrypted-payload pattern (§5), not by promises.

## §3. The four primitives in scope

| Primitive | Server | Closure | Conditional logic |
|---|---|---|---|
| Webcash secret | webcash.org (NOT us) | `/replace` | None — only conservation + namespace |
| Voucher secret | Voucher servers (we run; treat as protocol-frozen) | `/replace` | None |
| RGB record (fungible or collectible) | `webycash-server-rgb` (ours) | `/replace` (or `/transfer` for non-splittable) | AluVM HTLC + custom predicates per schema |
| Bitcoin ARK vtxo | ARK ASP (NOT us) | 2-of-2 multisig spend (taproot, MuSig2-aggregated) | Native HTLC, taproot script paths |

The whole cross-rail swap design works *given this set*. Webcash and Voucher are
dumb single-use-seal closure servers; we cannot extend them. The RGB server is ours
and fully programmable. ARK has native HTLC and taproot multisig.

## §4. Referee-ZKP-based swap — Webcash ↔ Bitcoin ARK

This is the **only flow that uses the referee**. The referee verifies both parties'
ciphertexts via Groth16 ZKPs (§9) without ever decrypting them, orchestrates the
encrypted-payload exchange, and co-signs the 2-of-2 MuSig2 vtxo. Settlement is
**race-bounded, not cryptographically atomic** (see §4.5). The referee is non-custodial
by construction (§5).

Bob holds webcash secret `S_B` with public hash `H_B = sha256(S_B)`. Alice holds an
ARK vtxo of equivalent value. Both have published PGP public keys. They've negotiated
price off-chain (extronet, social, whatever — out of scope for safety).

### 4.1. Setup

The vtxo is locked into a 2-of-2 MuSig2-aggregated taproot output: signers `(Alice, referee)`.
Two transactions are pre-defined at swap init:

- `TX_settle` — spends the vtxo to Bob's ARK pubkey. Authorisation: `Alice's partial-sig + referee's partial-sig`, MuSig2-aggregated.
- `TX_refund` — spends the vtxo back to Alice's ARK pubkey. Authorisation: `Alice's partial-sig + referee's partial-sig`, MuSig2-aggregated, but on a different signing session.

MuSig2 requires nonce exchange before partial-sigs can be produced. Alice and the
referee complete two nonce-exchange rounds at swap init: one for `TX_settle`, one for
`TX_refund`. Sessions are independent — same private keys, fresh nonces.

### 4.2. Pre-flight payloads

Alice produces:

- `EncSig_A_to_B` = `PGP_encrypt(Bob_pgp_pubkey, alice_partial_sig_on_TX_settle)`
- `ZKP_A` = Groth16 proof to the **referee** that `EncSig_A_to_B` decrypts under `Bob_pgp_privkey` to a valid MuSig2 partial-sig by Alice on `TX_settle`, against Alice's published MuSig2 pubkey-share. The referee verifies this proof; **neither Alice nor Bob is the verifier**.
- `alice_partial_sig_on_TX_refund` — held strictly locally by Alice. Never given to the referee, never given to Bob. (See §5 for why.)

Bob produces:

- `EncSec_B_to_A` = `PGP_encrypt(Alice_pgp_pubkey, S_B)`
- `ZKP_B` = Groth16 proof to the **referee** that `EncSec_B_to_A` decrypts under `Alice_pgp_privkey` to a 32-byte value `S` with `sha256(S) == H_B`, where `H_B` is the public hash the referee independently checked unspent at webcash.org.

Both payloads + both ZKPs go to the referee via authenticated HTTPS.

### 4.3. Settlement orchestration

The referee runs (all immutable steps; each produces a new typestate value, never mutates the prior):

1. **Verify** `ZKP_A` and `ZKP_B`. If either fails: abort, return signed receipt to both parties.
2. **Pre-check**: `webcash.org/api/v1/health_check([H_B])` returns `unspent`. Persist signed snapshot.
3. **Insert push**: invoke the configured push-webhook with `{ pgp_fp: Alice_fp, encrypted_payload: EncSec_B_to_A, type: "webcash", swap_id, callback_url }`. The push-notification service is **external** (§4.6) — the referee just calls a webhook. The push service is responsible for delivering to Alice's PWA. Alice's PWA service-worker receives the push and calls webylib's `insert_hook(Alice_pgp_pub, EncSec_B_to_A, type=webcash)`. The wallet implementor decrypts locally with Alice's PGP private key, recovers `S_B`, and immediately submits its own `/replace` to webcash.org transferring `H_B` to a fresh Alice-owned secret.
4. **Post-check**: `webcash.org/api/v1/health_check([H_B])` returns `spent` ⟹ Alice took ownership ⟹ swap **success**.
5. **Release to Bob**: referee transmits to Bob `(referee_partial_sig_on_TX_settle, EncSig_A_to_B)` over the same push channel. Bob's wallet decrypts `EncSig_A_to_B` with Bob's PGP private key, recovers `alice_partial_sig_on_TX_settle`, MuSig2-aggregates with `referee_partial_sig_on_TX_settle`, broadcasts `TX_settle` on ARK, claims the vtxo. **The referee at no point holds Alice's signature in cleartext — `EncSig_A_to_B` is forwarded as opaque bytes.**

### 4.4. Failure handling

If post-check (step 4) returns `unspent`, the insert-push didn't take effect. Two
possible reasons: push didn't deliver, or Alice's PWA didn't run yet, or there's a
slow webcash.org propagation. The referee retries step 3 + step 4 **up to 3 times**
with exponential backoff.

If 3 retries still see `unspent`, the swap **aborts**:

1. Referee invokes the push-webhook with `{ pgp_fp: Bob_fp, public_hash: H_B, type: "invalidate-webcash", swap_id, callback_url }`. Bob's PWA receives the push and calls webylib's `invalidate_hook(H_B)`. The wallet finds the matching secret in Bob's local store and `/replaces` it to a fresh Bob-owned secret. Even if `S_B` cleartext leaked at any point, it's now spent and worthless.
2. Once Bob's wallet acks the invalidation (signed receipt), referee transmits to Alice `referee_partial_sig_on_TX_refund` (cleartext — Alice is the recipient). Alice MuSig2-aggregates with her locally-held `alice_partial_sig_on_TX_refund`, broadcasts `TX_refund`, vtxo returns to Alice.

### 4.5. The race window we accept

Between the pre-check (`H_B` unspent) and the post-check (`H_B` spent), Bob *could*
front-run Alice's `/replace` by submitting his own `/replace` of `S_B` to a different
output. The webcash server is FIFO; whoever lands first wins.

If Bob front-runs, the post-check still sees `H_B` spent (just spent by Bob, not
Alice). The referee proceeds to release `TX_settle` to Bob, who gets the vtxo *and*
keeps the webcash. **Alice loses one swap's worth of webcash.**

We accept this risk because:

- The window is the time between pre-check and Alice's `/replace` landing — typically
  hundreds of milliseconds with a co-located referee.
- Alice's PWA has the `/replace` payload pre-built; only the decryption step is on the
  critical path after the push lands.
- Bob's only attack window is the same; he has no time advantage.
- Loss is bounded to the webcash side. Alice's vtxo is never at risk because it's
  locked behind the 2-of-2 multisig.

This is the **same race-with-pre-armed-wallet** property the original Webcash↔ARK
analysis identified. The referee tightens the window with co-location; it does not
eliminate the race. We document it honestly.

### 4.6. Push notification — out of scope

The referee does not run a push-notification service. It calls a configured webhook
URL with a typed JSON payload. The push provider (Web Push, FCM, APNs, custom) is
responsible for delivering the push to the recipient's PGP-fingerprint-registered
device. The push provider implements the webhook contract; the referee never
imports a push SDK.

Webhook contract (referee → push service):

```json
POST {push_webhook_url}
{
  "swap_id": "...",
  "recipient_pgp_fp": "<lowercase hex>",
  "kind": "insert" | "invalidate" | "release-settle",
  "payload": "<base64 ciphertext or hash, depending on kind>",
  "callback_url": "https://referee.example/v1/swap/{id}/ack"
}
```

The push service is expected to call `callback_url` once the recipient's wallet
acknowledges processing. Anything beyond that contract is the push provider's
operational concern.

## §5. Why we never custody a signature or a secret

The cryptographic non-custody property of the referee is enforced by **two invariants
that hold across every step of every swap**:

1. **The referee never holds Alice's `TX_settle` partial-sig in cleartext.**
   Alice gives it encrypted to Bob's PGP pubkey. The referee can verify (via Alice's
   ZKP) that the ciphertext is honest, but cannot decrypt it.
2. **The referee never holds Alice's `TX_refund` partial-sig at all.**
   Alice keeps it strictly local.

Both invariants are required under MuSig2's threat model. MuSig2 partial-sigs share
a per-session nonce; if the referee held Alice's TX_settle partial *and* TX_refund
partial at the same time in cleartext, with their corresponding nonces, the referee
could recover Alice's MuSig2 private-share via standard nonce-reuse algebra. The
encrypted-to-Bob blob protects invariant (1); strict locality protects invariant (2).

Symmetrically for Bob's webcash secret:

3. **The referee never holds `S_B` in cleartext.**
   Bob gives it encrypted to Alice's PGP pubkey. The ZKP attests honesty without revealing `S_B`.

These three invariants are checked in code (typestate transitions in §10 forbid
constructing any state that would violate them). They are also checked by the audit
log: every signed message the referee emits commits to the *ciphertext bytes it
forwarded*, not to any cleartext.

## §6. Webcash ↔ Voucher — out of scope

Vouchers are bought directly from the voucher issuer. There is no useful cross-rail
swap between them and webcash that the issuer cannot serve directly. We do not ship
this flow, and we do not document a "future-work" path for it. If a use-case ever
materialises, it will be designed from first principles, not retrofitted from this
doc.

## §7. RGB ↔ {Webcash, Voucher, Bitcoin ARK} — HTLC, no referee

The RGB server is programmable. AluVM HTLC on the RGB side gives one cryptographic
half of every swap. The other half is the bearer-cash race (Webcash, Voucher) or
native HTLC (ARK).

### 7.1. RGB ↔ Bitcoin ARK — fully cryptographic

Both sides program the same hashlock `H = sha256(X)`:

| # | Actor | Server | Operation |
|---|---|---|---|
| 0 | Bob | local | Pick `X`, `H = sha256(X)`. Send `H` to Alice. |
| 1 | Bob | RGB server | `/replace` + `htlc_locks`: lock RGB record with `committed_h=H, claim_owner=Alice, refund_owner=Bob, refund_after_seconds_from_now=3600`. |
| 2 | Alice | ARK ASP | New vtxo: claim by `Bob_pubkey + preimage(H)`; refund to Alice after `T_A < 3600`. |
| 3 | Both | local | WASM-AluVM verify each other's lock state. |
| 4 | Bob | ARK ASP | Spend vtxo, claim path. Reveals `X` on ARK. |
| 5 | Alice | RGB server | `/replace` + `htlc_witnesses`: `provided_x_hex=X`. AluVM accepts. Alice owns RGB record. |

Atomicity: cryptographic. Either both legs settle, or both refund after their
timeouts. No referee, no custody, no race.

### 7.2. RGB ↔ Webcash — half-cryptographic, encrypted-payload pattern

The RGB side uses HTLC. The webcash side uses the same encrypted-payload + insert_hook
mechanism as §4, **decoupled from the HTLC preimage**.

| # | Actor | Server | Operation |
|---|---|---|---|
| 0 | Bob | local | Pick HTLC preimage `X` (32B), `H = sha256(X)`. Pick a separate AES key `K` (32B). Compute `EncSec_B_to_A = PGP_encrypt(Alice_pgp_pubkey, S_B)`. Send `H` and `EncSec_B_to_A` to Alice. |
| 1 | Alice | RGB server | `/replace` + `htlc_locks`: lock her RGB asset with `committed_h=H, claim_owner=Bob, refund_owner=Alice, refund_after_seconds_from_now=1800`. |
| 2 | Both | local | WASM-AluVM verify lock parameters. |
| 3 | Bob | RGB server | `/replace` + `htlc_witnesses`: `provided_x_hex=X`. AluVM accepts; Bob owns the RGB asset. **`X` is now public on the RGB audit log.** |
| 4 | Alice | local + webcash.org | Push delivers `EncSec_B_to_A` to Alice's PWA via `insert_hook(Alice_pgp_pub, EncSec_B_to_A, type=webcash)`. PWA decrypts with Alice's PGP **private key** (NOT `X`), recovers `S_B`, races a `/replace` to webcash.org. |

Critical decoupling: **`X` (the HTLC preimage) is independent of the webcash secret
encryption.** The original §4.D scheme that derived the webcash AES key from `X` was
broken — once Bob revealed `X` in step 3, the audit log made the webcash secret
extractable by anyone watching the RGB server. Under the corrected scheme, `X` only
gates the RGB transition; the webcash secret is encrypted to Alice's PGP pubkey
end-to-end and only Alice's PGP private key (held in her PWA) can decrypt it.

The race window in step 4 is the same race-with-pre-armed-wallet as §4.5 — bounded,
documented, accepted.

### 7.3. RGB ↔ Voucher

Mechanically identical to §7.2 with `type=voucher` and the relevant voucher server
in step 4. No referee.

## §8. Time source for HTLC

AluVM has no native clock. Three options exist; we pick the third:

1. **Wallet clock** — wallet sends `current_unix` in the witness. Rejected: a malicious wallet posts the future and bypasses timeout.
2. **External oracle** — server pulls signed time from NTP/timestamp service. Adds dependency, doesn't escape the operator-trust assumption.
3. **Server's own wall clock** — server reads `SystemTime::now()` at the moment of `/replace` and overrides whatever the wallet sent. Selected.

Consequence: the clock you trust is the operator you already trust to run `/replace`
honestly. No new trust assumption. If the operator post-dates the clock, it's
already dishonest enough to fake `/replace` outcomes directly.

Wallet pre-flight uses `chrono::Utc::now()` locally for UX (showing time-to-refund);
small skew is harmless because the server is the source of truth.

**Lock-time discipline**: the wallet never supplies an absolute `refund_after_unix`.
It supplies a *delta* (seconds from now). The server stamps
`state.refund_after_unix = server_now + delta` into the output record at lock time.
Source of truth is unambiguously the server's clock at lock time.

## §9. ZKP scope

ZKPs in the webycash family exist for one purpose: **the referee verifies, without
seeing cleartext, that an encrypted payload is honest.** ZKPs are never delivered to
Alice or Bob; they are a referee-side gate.

Two circuits in scope:

- **Bob's ZKP**: proves `PGP_decrypt(EncSec_B_to_A, Alice_pgp_privkey) = S_B ∧ sha256(S_B) = H_B`. Witnesses: `S_B`, `Alice_pgp_privkey` (well, the symmetric session key extracted by hybrid PGP). Public inputs: `EncSec_B_to_A`, `H_B`, Alice's PGP pubkey.
- **Alice's ZKP**: proves `PGP_decrypt(EncSig_A_to_B, Bob_pgp_privkey) = sig ∧ MuSig2_partial_verify(sig, alice_pubkey_share, TX_settle, session_nonces) = ok`. Witnesses: `sig`, the symmetric session key. Public inputs: `EncSig_A_to_B`, `TX_settle` hash, Alice's MuSig2 pubkey-share, the agreed nonces.

**Scheme**: Groth16. Concrete circuit compilation, choice of curve cycle, and WASM
toolchain are deferred to milestone M-2 verification — secp256k1 is not a SNARK-
friendly curve, so either we use a curve cycle (BLS12-377/Jubjub) and prove a
"virtual" secp256k1 verification, or we eat the cost of secp256k1-in-SNARK. Either is
feasible; cost depends on circuit constraints. Toolchain target: `arkworks` or
`circom + snarkjs`, whichever compiles cleanly to WASM with no C dependencies. This
must be validated empirically before referee implementation locks in the protocol.

## §10. Referee state — typestate, not mutation

Each swap in the referee is a sequence of immutable values, each typed by its phase:

```
SwapInit              — created from the inbound /v1/swap/initiate request
  ⟶ verify_zkps()  → ZkpsVerified
ZkpsVerified
  ⟶ pre_check()    → PreChecked
PreChecked
  ⟶ insert_push()  → InsertPushed
InsertPushed
  ⟶ post_check()   → PostChecked { spent: true }  → Settled
                  ⟶ PostChecked { spent: false } → retry up to 3
Retried(3)
  ⟶ invalidate_push() → Invalidated
Invalidated
  ⟶ refund_push()     → Refunded
```

In Rust:

```rust
pub struct SwapState<P: Phase> {
    swap_id: SwapId,
    parties: Parties,        // pgp_fps, MuSig2 pubkey-shares
    payloads: Payloads,      // ciphertexts, ZKPs (verified or not depending on phase)
    audit: AuditChain,       // append-only signed audit; new value is appended,
                              // returning a fresh AuditChain — the prior chain
                              // is immutably referenced.
    _phase: PhantomData<P>,
}

pub struct SwapInit;
pub struct ZkpsVerified;
pub struct PreChecked;
// ... one type per phase
```

Transitions are pure functions: `fn verify_zkps(s: SwapState<SwapInit>) -> Result<SwapState<ZkpsVerified>, AbortReason>`. They consume the prior state and return a new state. No transition mutates `s` in place. The phantom `Phase` parameter prevents calling `post_check` before `insert_push` at compile time.

Strict-types schemata define the wire format and on-disk persistence of `SwapState`
(canonical bytes for audit log signing). AluVM is not used inside the referee itself
— it's used on the RGB server side when the referee mints the on-chain swap-record
RGB21 contract that publicly commits the swap parameters.

## §11. Webycash-server cargo features

The `webycash-server` workspace defaults to **Webcash only** — frozen wire format,
no RGB, no Voucher, no referee. Every other rail is opt-in via cargo features.

```toml
[features]
default = []                                      # webcash only
rgb     = ["dep:asset-rgb", "asset-core/rgb-hooks"]
voucher = ["dep:asset-voucher"]
referee = ["rgb", "dep:referee-core"]            # referee mints a swap RGB21 record;
                                                  # so referee depends on rgb.
```

Build matrix (all must compile clean, all must `cargo test`):

- `cargo build` → webcash-only binary (`server-webcash`).
- `cargo build --features rgb` → adds `server-rgb` binary.
- `cargo build --features voucher` → adds `server-voucher` binary.
- `cargo build --features rgb,voucher` → all three asset binaries.
- `cargo build --features referee` → all three asset binaries + `referee` binary.

CI gate: matrix of `{(), (rgb), (voucher), (rgb,voucher), (rgb,voucher,referee)}` ×
`{linux-x86_64, linux-arm64, freebsd-x86_64}`, plus `wasm32` for the AluVM runtime
and webylib WASM target.

The `referee` crate is a workspace member at `webycash-server/referee/`, gated
behind the `referee` feature so a Webcash-only deployer never compiles it.

## §12. Limitations

- **Webcash and Voucher servers cannot enforce conditional transfer.** Their settlement primitive is single-use-seal closure on a single hash; cross-rail conditionality is the wallet/referee's responsibility, not theirs.
- **Settlement of any flow with a webcash/voucher leg is race-bounded** (§4.5, §7.2). Pre-armed wallets and co-located referee tighten the race; they do not eliminate it. Loss bounded to the bearer-cash leg of one swap.
- **Settlement of RGB ↔ ARK is fully cryptographic** (§7.1). Either both legs settle or both refund after their timeouts.
- **The RGB server is trusted for double-spend prevention AND for AluVM execution.** A rogue operator can deny service or rewrite history; auditors re-running AluVM independently from the audit log catch the latter.
- **The referee is non-custodial** (§5) but is trusted for liveness (delivering pushes, releasing the second signature). A malicious referee cannot steal funds; it can only delay the swap and force timeout-refunds.
- **Push notification delivery is out of scope** (§4.6). Operational concern of the deployer; we ship the webhook contract.
- **ZKP toolchain feasibility is an open milestone gate** (§9). If Groth16 over the chosen curve cycle has unacceptable constraint count or WASM bundle size, we revisit before locking the protocol.
- **Webcash ↔ Voucher swap is permanently out of scope** (§6).

## §13. What this document is NOT

- Not a claim that webcash.org's protocol changes. It does not.
- Not a claim that webcash is trustless. It is trusted-server bearer cash; that is the model and we live within it.
- Not a claim our RGB server matches RGB-on-Bitcoin's trust model. RGB-on-Bitcoin is trustless single-use seals; our RGB server is a server-mediated approximation — useful for fast cheap contracts, weaker than Bitcoin-anchored RGB.

The goal is *honest engineering of what's possible given the rails we have*.
Document everything. Over-promise nothing.
