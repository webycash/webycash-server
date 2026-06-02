# Cross-rail pair policy

Status: coordination document. No protocol change. This file restates the
asset-pair rules that already govern the webycash family so that
`webycash-server` (asset ledgers + traits) and `webycash-referee` (the
production swap mediator) share one authoritative matrix.

Sources of truth this file aligns to — it invents nothing:

- `extro/docs/development/24-extro-402-scheme-and-webycash-plan.md` (asset
  model + pair policy).
- `docs/referee-zkp-based-swap.md` §6 (Webcash ↔ Voucher out of scope) and
  §7 (RGB ↔ {Webcash, Voucher, ARK}).
- `crates/asset-core/src/lib.rs` (`ReplaceHook`, `SplittableAsset`,
  `TransferableAsset`) and `crates/asset-rgb/src/htlc/*` (the server-side
  HTLC predicate gate).

## §1. The four rails and their settlement primitive

| Rail | Server | Closure primitive | Conditional logic available |
|---|---|---|---|
| Webcash | webcash.org (NOT us) | `/replace` (single-use-seal) | None — conservation + namespace only |
| Voucher | voucher servers (we run; protocol-frozen) | `/replace` | None |
| RGB20 / RGB21 | `webycash-server-rgb` (ours) | `/replace` (RGB20 splittable) / 1:1 `/replace` (RGB21) | AluVM HTLC predicate + per-schema checks |
| Bitcoin ARK vtxo | ARK ASP (NOT us) | 2-of-2 MuSig2 taproot spend | Native HTLC, taproot script paths |

The asymmetry that drives the whole design (`referee-zkp-based-swap.md` §0):
**bearer cash transfers a secret; ARK transfers a signature; RGB transitions
state under AluVM.** A pair is safe to swap only when at least one leg can be
made *arbitrable* (referee co-signature) or *conditional* (RGB/ARK HTLC). Two
bearer legs have neither, so they are forbidden.

## §2. Pair matrix

| Pair | Allowed? | Mechanism | Settlement property |
|---|---|---|---|
| Webcash ↔ Voucher | **Forbidden** | — | Neither side is arbitrable or conditional; nothing to enforce. See §3. |
| Webcash ↔ Bitcoin ARK | Allowed, **referee required** | Referee-ZKP-based swap (`referee-zkp-based-swap.md` §4) | Race-bounded, not atomic (§4.5) |
| Voucher ↔ Bitcoin ARK | Allowed, **referee required** | Same referee flow with `type=voucher` | Race-bounded, not atomic |
| RGB20/RGB21 ↔ Bitcoin ARK | Allowed, **no referee** | HTLC on both legs, same hashlock `H` (`§7.1`) | Fully cryptographic — both settle or both refund |
| RGB20/RGB21 ↔ Webcash | Allowed (**RGB leg is the conditional side**), no referee | HTLC on RGB + encrypted-payload bearer race (`§7.2`) | Half-cryptographic: RGB leg cryptographic, webcash leg race-bounded |
| RGB20/RGB21 ↔ Voucher | Allowed (**RGB leg is the conditional side**), no referee | Same as RGB↔Webcash, `type=voucher` (`§7.3`) | Half-cryptographic, race-bounded on the voucher leg |
| RGB20/RGB21 ↔ RGB20/RGB21 | Allowed, **no referee** | RGB-server-enforced conditional state transition / shared-hashlock HTLC | Server enforces the condition directly |

Single governing invariant (extro-402 plan, asset-model §): *every cross-rail
exchange that involves a bearer asset (Webcash or Voucher) must have at least
one referee-arbitrable or contract-arbitrable leg — Bitcoin ARK or RGB.* The
two forbidden / referee-mandatory rows fall straight out of it.

## §3. Webcash ↔ Voucher — forbidden, not "future work"

Both legs are bearer-only single-use-seal closures with no conditional script
and no co-signable multisig. A mediator cannot enforce a safe release on
*either* side, so there is no construction (referee or otherwise) that makes
the pair safe. Vouchers are also bought directly from their issuer, so a
cross-rail swap against webcash serves no purpose the issuer cannot serve
directly (`referee-zkp-based-swap.md` §6). This pair is permanently rejected;
we ship no flow and no future-work path for it.

## §4. Bearer ↔ ARK — the only referee pair

Webcash ↔ ARK and Voucher ↔ ARK are the only flows that use the referee. The
referee verifies both parties' ciphertexts via Groth16 ZKPs (it is never the
plaintext recipient), co-signs the 2-of-2 MuSig2 vtxo, and orchestrates the
encrypted-payload exchange. It is **non-custodial by construction**
(`referee-zkp-based-swap.md` §5) and trusted only for liveness — a malicious
referee can delay a swap to timeout-refund but cannot steal. Settlement is
**race-bounded, not atomic**: between the referee's pre-check (`H` unspent) and
the post-check (`H` spent), the bearer-secret holder could front-run; loss is
bounded to one swap's worth of the bearer leg, and the ARK leg is never at risk
behind the 2-of-2 (§4.5).

The production mediator for this pair is the **`webycash-referee`** repository
(native Rust serverless function). See `docs/referee-production-coordination.md`.

## §5. RGB conditional flows — server enforces the condition

The RGB server is ours and programmable, so the HTLC half of any RGB swap is
enforced on the server, not by a mediator.

- **RGB ↔ ARK (§7.1)** — both legs program the same hashlock `H = sha256(X)`.
  Revealing `X` to claim one leg exposes it for the other. Fully cryptographic;
  no referee, no custody, no race.
- **RGB ↔ Webcash / Voucher (§7.2 / §7.3)** — the RGB leg is HTLC-gated; the
  bearer leg uses the same encrypted-payload + `insert_hook` mechanism as the
  referee flow, but **decoupled from the HTLC preimage** (the webcash secret is
  PGP-encrypted to the recipient end-to-end; `X` only gates the RGB
  transition). The bearer leg is race-bounded (same property as §4.5); the RGB
  leg is cryptographic. No referee — the RGB server is the conditional party.
- **RGB ↔ RGB** — both outputs live on our RGB server, so the server can
  enforce the swap condition (shared hashlock HTLC, or a paired conditional
  transition) directly. A referee is **not required** and must not be inserted.

### How the server gate works (no protocol change here)

`crates/asset-core` models the rail distinction with `ReplaceHook`:

- Webcash and Voucher use the **default no-op** `ReplaceHook` — their
  `/replace` is a pure single-use-seal closure (`asset-core/src/lib.rs`,
  `ReplaceHook` default impls return `Ok(())`). They are **non-conditional by
  design** and the server has no hook to lock them.
- RGB flavors **override** `ReplaceHook` to run the HTLC predicate
  (`crates/asset-rgb/src/htlc/predicate.rs`, gated through
  `vm::execute_predicate`). Once an output carries `HtlcState`, the only valid
  `/replace` is one that satisfies the claim path (`sha256(X) == committed_H`
  and the output owner equals the pre-committed `claim_owner_hash`) or the
  refund path (server clock past `refund_after_unix` and output owner equals
  `refund_owner_hash`).

**Time discipline**: the wallet supplies only a *delta*
(`refund_after_seconds_from_now`); the server stamps
`refund_after_unix = server_now + delta` at lock time
(`asset-rgb/src/htlc/state.rs`, `LockRequest::stamp_into_state`). The clock you
trust is the operator you already trust to run `/replace`
(`referee-zkp-based-swap.md` §8) — no new trust assumption.

## §6. What the server cannot do — hard limits (no workarounds)

- **Webcash and Voucher cannot be made conditionally lockable.** Their
  settlement primitive is single-use-seal closure on one hash; there is no
  request extra, header, or feature that turns a bearer `/replace` into a
  conditional one. Attempts to model conditional locking on these rails are out
  of scope and must be rejected at review. Cross-rail conditionality on a
  bearer leg is the wallet/referee's responsibility, never the bearer server's.
- **Any flow with a bearer leg is race-bounded**, not atomic. Pre-armed wallets
  and a co-located referee tighten the race window; they do not remove it. Loss
  is bounded to the bearer leg of one swap.
- **RGB ↔ ARK is the only fully-cryptographic cross-rail flow.**
- The RGB server is trusted for double-spend prevention and AluVM execution; a
  rogue operator can deny service or rewrite history, and independent auditors
  re-running the predicate from the audit log catch the latter
  (`referee-zkp-based-swap.md` §12–§13).

## §7. Where pair-policy tests live

Per the coordination plan (`docs/development-plan-cloudflare-rgb-adapters.md`),
pair-policy enforcement tests belong in the Extro / PWA / referee repos, not in
asset-server core — except tests that validate *server-specific* behavior. The
asset server's own conformance suite (`crates/conformance/`) covers the
primitives this policy relies on: Webcash lifecycle, Voucher issue/replace, and
RGB20 / RGB21 HTLC lock / claim / refund. Reference tests that would document
"Webcash/Voucher `ReplaceHook` is a no-op" and "RGB HTLC rejects missing
witness / accepts correct preimage / accepts refund after timeout" are a
**later phase** and are not added here.
