# Referee — architecture

## Why this service exists

The referee mediates a single, narrow flow: **Webcash ↔ Bitcoin ARK
swaps** (`webycash-server/docs/referee-zkp-based-swap.md` §4). Webcash
transfers a *secret*; ARK transfers a *signature*. Neither rail has a
primitive the other understands. The referee glues them together by:

1. Verifying both parties' encrypted payloads via Groth16 ZKPs **without
   ever decrypting them**.
2. Calling webcash.org's `/api/v1/health_check` to pre-check (`H_B`
   unspent before insert) and post-check (`H_B` spent after insert).
3. Co-signing the 2-of-2 MuSig2 vtxo so Bob can claim it on success, and
   so Alice can refund it on failure.
4. Emitting a public, signed audit log of every action it takes.

It is **non-custodial**. It never holds Alice's `TX_settle` MuSig2
partial-sig in cleartext, never holds her `TX_refund` partial-sig at all,
and never holds Bob's webcash secret in cleartext. The only secrets it
owns are its own Ed25519 identity key and its own MuSig2 key share.

## Components

```
┌─────────────────────────── referee binary ───────────────────────────┐
│                                                                       │
│  ┌─────────────────┐   ┌──────────────────┐   ┌──────────────────┐    │
│  │ HTTP API (axum) │──▶│   Orchestrator   │──▶│  Pure typestate  │    │
│  │  /v1/* endpoints│   │ (api/orchestrator│   │  (state/{phases, │    │
│  └─────────────────┘   │   .rs)           │   │   transitions,   │    │
│                        │                   │   │   types}.rs)     │    │
│                        └─────────┬────────┘   └──────────────────┘    │
│                                  │                                     │
│           ┌──────────┬───────────┼──────────┬───────────┐              │
│           ▼          ▼           ▼          ▼           ▼              │
│      ┌────────┐ ┌────────┐ ┌─────────┐ ┌────────┐ ┌──────────┐        │
│      │ Identity│ │  ZKP  │ │ MuSig2  │ │ Push   │ │ Webcash  │        │
│      │  (Ed25519│ │Verifier│ │ Signer  │ │Trans-  │ │  + RGB   │        │
│      │  signing)│ │ (trait)│ │ (trait) │ │port    │ │  clients │        │
│      └────────┘ └────────┘ └─────────┘ │ (trait)│ │ (traits) │        │
│                                         └────────┘ └──────────┘        │
│                                                                       │
│   ┌────────────────┐    ┌────────────────┐                            │
│   │ Audit log      │    │ Swap-state     │                            │
│   │ (append-only,  │    │ store          │                            │
│   │  signed chain) │    │ (in-mem / pg)  │                            │
│   └────────────────┘    └────────────────┘                            │
│                                                                       │
└──────────────────────────────────────────────────────────────────────┘
            ▲                          │
            │                          ▼
   wallet implementor          push provider (out of scope)
   (extro-node, harmoniis      ────────────────────────────
   wallet, PWA, etc.)          delivers `insert_hook`,
                               `invalidate_hook`,
                               `release_settle`,
                               `release_refund` payloads
                               to recipient devices over
                               Web Push / FCM / APNs.
```

## Trust composition

Per `referee-zkp-based-swap.md §2`:

- **L1 user keys** — held by users; never reach the referee.
- **L2 trusted code** — webylib, AluVM bytecode, RGB-server `ReplaceHook`
  validators. Reproducible builds; deterministic transitions; public
  source.
- **L3 trusted services (us)** — `webycash-server-rgb`, this `referee`
  binary. Public Ed25519 pubkey, signed audit logs, capped action surface.
- **L4 external services** — webcash.org, voucher servers, Bitcoin ARK
  ASP, push-notification provider. Honest-but-fixed.
- **L5 counterparty** — adversarial.

## Typestate (immutability core)

Phase transitions are pure functions, not mutations:

```text
SwapInit
   │ verify_zkps   (consumes the prior value, returns a new one)
   ▼
ZkpsVerified
   │ pre_check
   ▼
PreChecked
   │ insert_pushed_from_pre
   ▼
InsertPushed ──── insert_pushed_retry (≤ N) ──┐
   │                                          │
   │ post_check spent                         │ post_check still unspent
   ▼                                          │
Settled (terminal)                            │ retries exhausted
                                              ▼
                                          Aborted
                                              │ invalidated
                                              ▼
                                         Invalidated
                                              │ refunded
                                              ▼
                                          Refunded (terminal)
```

Each phase is a separate Rust ZST in `state/phases.rs`. `SwapState<P>`
carries the same payload across phases; only the phantom `P` changes.
This rules out invalid sequences (calling `post_check` before
`insert_push`, settling without verifying ZKPs, etc.) at compile time.

## Pluggable collaborators

Every external interaction is a trait so tests use mocks and production
plugs in real implementations:

| Trait | File | Real implementation feature |
|---|---|---|
| `Verifier` | `zkp.rs` | `zkp-arkworks` (Groth16/BN254) |
| `Musig2Signer` | `musig2.rs` | `musig2-real` (musig2 + secp256k1) |
| `PushTransport` | `push.rs` | `HttpPush` (always available) |
| `WebcashClient` | `clients/mod.rs` | wired in production main.rs |
| `RgbClient` | `clients/mod.rs` | wired in production main.rs |
| `AuditLog` | `audit.rs` | `InMemoryAuditLog` default; Postgres opt-in |
| `SwapStore` | `store/mod.rs` | `InMemoryStore` default; Postgres via `postgres` |

Tests in `tests/orchestrator_e2e.rs` exercise the full orchestrator with
all-mock collaborators (settlement, abort, ZKP rejection, pre-check
rejection, audit chain integrity, store persistence). Lib unit tests in
each module exercise the individual pieces.

## What lives where

```
referee/
├── Cargo.toml             — opt-in workspace member; features `zkp-arkworks`,
│                            `musig2-real`, `postgres`
├── src/
│   ├── lib.rs             — module map + re-exports
│   ├── main.rs            — production binary entry point
│   ├── error.rs           — RefereeError + Result<T>
│   ├── config.rs          — Config::from_env
│   ├── sign.rs            — Ed25519 identity, canonical-message signing
│   ├── state/
│   │   ├── mod.rs
│   │   ├── phases.rs      — Phase ZSTs (SwapInit, ZkpsVerified, …)
│   │   ├── transitions.rs — pure transition fns
│   │   └── types.rs       — payload types (PgpEncrypted<T>, Groth16Proof, …)
│   ├── zkp.rs             — Verifier trait + MockVerifier + (optional) Arkworks
│   ├── musig2.rs          — Musig2Signer trait + MockSigner + (optional) real
│   ├── push.rs            — PushTransport trait + HttpPush + MockPush
│   ├── audit.rs           — AuditLog trait + InMemoryAuditLog
│   ├── store/mod.rs       — SwapStore trait + InMemoryStore + MockStore
│   ├── clients/mod.rs     — WebcashClient + RgbClient traits + mocks
│   └── api/
│       ├── mod.rs
│       ├── orchestrator.rs — Orchestrator::{start_swap, run_swap}
│       └── router.rs       — axum routes
├── tests/
│   └── orchestrator_e2e.rs — settlement / abort / rejection / audit / store
└── docs/
    ├── architecture.md     — this file
    ├── api.md              — endpoint specs
    ├── musig2-ceremony.md  — exact MuSig2 protocol
    ├── zkp-circuits.md     — Groth16 circuit specs
    ├── push-notification.md — webhook contract
    ├── hook-contract.md    — `insert_hook` / `invalidate_hook` for extro-node
    ├── trust-model.md      — formal threat model
    └── deployment.md       — KMS / Postgres / boot sequence
```

## Immutability discipline (cross-references)

The "data becomes, doesn't move" directive applies throughout:

- `SwapState<P>` is `Clone + Serialize`; transitions consume the prior
  value and return a new one.
- `PgpEncrypted<T>` is a newtype — referee holds `Vec<u8>` + a phantom
  marker recording what the cleartext *would be*; never decrypts.
- The audit log appends entries; never modifies them.
- The push transport is a trait method that takes `&self` + `&PushRequest`
  — the request is never mutated by the transport.
- The Orchestrator exposes two entry points: `run_swap(id, …)` runs the
  full state machine inline (used by tests) and `start_swap(…)` generates
  a fresh `SwapId`, persists an `accepted` placeholder, then
  `tokio::spawn`s `run_swap` and returns the id immediately. The HTTP
  `/v1/swap/initiate` handler always uses `start_swap` so the request
  doesn't block on the multi-second post-check loop. Consumers learn
  terminal state through `/v1/swap/{id}/poll` or
  `/v1/swap/{id}/audit`.
