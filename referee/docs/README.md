# Referee design docs

The `referee` binary mediates **Webcash ↔ Bitcoin ARK** swaps using a
non-custodial protocol: both parties' encrypted payloads are verified
via Groth16 ZKPs against the public hashes the audit chain commits to,
MuSig2 2-of-2 partial-signatures gate vtxo settlement and refund, and
every phase transition is committed to a public signed audit log.

The protocol it implements is fully specified in
[`webycash-server/docs/referee-zkp-based-swap.md`](../../docs/referee-zkp-based-swap.md)
§4. The docs in this folder are the **implementation specification**
that `referee/src/` realises.

| Doc | Topic |
|---|---|
| [`architecture.md`](architecture.md) | Components, typestate diagram, file layout, immutability discipline |
| [`api.md`](api.md) | Every HTTP endpoint with request/response shapes + error mapping |
| [`musig2-ceremony.md`](musig2-ceremony.md) | Exact MuSig2 2-of-2 protocol, nonce handling, cryptographic invariants |
| [`zkp-circuits.md`](zkp-circuits.md) | Groth16 circuit specs for both verifications + toolchain choices |
| [`push-notification.md`](push-notification.md) | Webhook contract for the external push provider |
| [`hook-contract.md`](hook-contract.md) | `insert_hook` / `invalidate_hook` spec for wallet implementors (extro-node) |
| [`trust-model.md`](trust-model.md) | Formal threat model, what the referee can/cannot do |
| [`deployment.md`](deployment.md) | Build, env vars, Postgres schema, KMS, upgrades |

## How the docs relate

```
                     ┌────────────────────────────────────┐
                     │ referee-zkp-based-swap.md (proto)  │
                     │  the protocol — what we promise    │
                     └────────────────┬───────────────────┘
                                      │
              ┌───────────────────────┼─────────────────────────┐
              │                       │                         │
              ▼                       ▼                         ▼
    ┌─────────────────┐     ┌──────────────────┐     ┌───────────────────┐
    │ architecture.md │     │ trust-model.md   │     │ hook-contract.md  │
    │ how it's built  │     │ what holds; why  │     │ wallet-side spec  │
    └────┬────────────┘     └──────────────────┘     └───────────────────┘
         │
   ┌─────┼──────────┬───────────────┬───────────────┐
   ▼     ▼          ▼               ▼               ▼
 api.md  musig2-ceremony.md   zkp-circuits.md  push-notification.md
 HTTP    cryptographic        Groth16 circuit  webhook contract
 spec    nonce + sig flow     specs            (push provider)

                                         deployment.md
                                         build, env, KMS, ops
```

## Implementation status

| Component | Status | Notes |
|---|---|---|
| `Identity` (Ed25519 signing) | Complete | `src/sign.rs`; 4 unit tests |
| `state::*` typestate | Complete | `src/state/`; 7 unit tests covering every transition + reject path |
| `audit::*` (signed append-only) | Complete | `src/audit.rs`; 3 unit tests including chain integrity |
| `zkp::Verifier` trait + `MockVerifier` | Complete | `src/zkp.rs`; 2 unit tests |
| `zkp::ArkworksVerifier` (real Groth16) | Stubbed behind `zkp-arkworks` feature | M-4 follow-up: wire actual circuit verification once extro-node ships circuit fixtures |
| `musig2::Musig2Signer` trait + `MockSigner` | Complete | `src/musig2.rs`; 2 unit tests |
| `musig2::RealSigner` | Stubbed behind `musig2-real` feature | Production wiring pairs with extro-node integration |
| `push::PushTransport` trait + `HttpPush` + `MockPush` | Complete | `src/push.rs`; 3 unit tests |
| `clients::{WebcashClient, RgbClient}` traits + mocks | Complete | `src/clients/`; 2 unit tests |
| `store::SwapStore` trait + `InMemoryStore` + `MockStore` | Complete | `src/store/`; 3 unit tests |
| Postgres-backed `SwapStore` (production) | Stubbed behind `postgres` feature | Schema in `deployment.md`; sqlx wiring is a one-evening job once dev compose lands |
| `api::Orchestrator::run_swap` (typestate runner) | Complete | `src/api/orchestrator.rs`; 7 e2e tests in `tests/orchestrator_e2e.rs` |
| `api::Orchestrator::start_swap` (background spawn) | Complete | Generates a fresh `SwapId`, persists `accepted` placeholder, `tokio::spawn`s `run_swap`, returns id immediately |
| `api::router` (axum surface) | Complete | `src/api/router.rs`; `/v1/swap/initiate` returns immediately with `swap_id` + `status="accepted"`; clients poll `/v1/swap/{id}/poll` |
| `main.rs` (production entry) | Complete | `src/main.rs`; loads identity, wires Orchestrator, refuses to boot with mock crypto unless `REFEREE_ALLOW_MOCK_CRYPTO=1` |
| Webhook HMAC | Audited (`hmac` crate) | `src/push.rs`; replaces earlier hand-rolled HMAC with the RustCrypto crate |
| Documentation | Complete | This folder |

Repo-level docs that reference the referee:

- [`webycash-server/docs/deployment.md`](../../docs/deployment.md) — full-stack production deployment (all 4 asset binaries + referee).
- [`webycash-server/docs/roadmap.md`](../../docs/roadmap.md) — what ships now vs what is gated on real-crypto wiring.
- [`webycash-server/docs/referee-zkp-based-swap.md`](../../docs/referee-zkp-based-swap.md) — the protocol spec.

## Test coverage

```
referee/src — 25 unit tests
referee/tests/orchestrator_e2e.rs — 7 integration tests with full mocks:
  ✓ settlement happy path delivers release-settle to bob
  ✓ abort path invalidates bob then refunds alice
  ✓ zkp rejected short-circuits before pre-check
  ✓ pre-check already spent short-circuits
  ✓ audit log chain is well-formed on happy path
  ✓ store reflects terminal phase on settled path
  ✓ store reflects terminal phase on refunded path
```

For end-to-end testing against real RGB / Webcash servers, see
`webycash-server/crates/conformance/` (the conformance crate runs the
HTTP layer against actual server binaries; the referee plugs in once
its production cryptographic primitives are wired). The conformance
suite currently covers the HTLC primitive on RGB Fungible (5 tests)
and RGB Collectible (5 tests), which the referee depends on for its
swap-tracking record.

## What lives elsewhere

The referee implements **only the server side** of the swap. The
following live in other projects:

- **PGP encryption / decryption** — wallet implementor (extro-node).
- **Groth16 proving** — wallet implementor (proves; this crate verifies).
- **MuSig2 partial-sig generation by Alice** — wallet implementor.
- **Bitcoin ARK transaction construction + broadcast** — wallet implementor.
- **`insert_hook` / `invalidate_hook` callbacks** — wallet implementor; spec in `hook-contract.md`.
- **Push notification delivery** — operator's chosen push provider; webhook spec in `push-notification.md`.
- **Webcash server** — webcash.org, NOT us; we only call `/api/v1/health_check`.
- **RGB server** — `webycash-server-rgb` (sibling crate); we mint a swap-tracking RGB21 record on it per swap.
