# webycash-server — roadmap & production-readiness status

Honest answer to "can we go to production?": **partial.** The
asset-flavour servers are production-ready; the `referee` swap helper
is not — it works end-to-end with mocks, but two integration gaps are
still open before it can mediate real Webcash↔ARK swaps. They are
listed in detail under "Gated on" below.

## What can ship now

| Capability | Status |
|---|---|
| `webycash-server-webcash` (Webcash wire-protocol-frozen) | **Ship.** Conformance suite green against webcash.org behaviour. |
| `webycash-server-rgb` (RGB20 + AluVM-validated `replace`) | **Ship.** Asset-rgb HTLC primitive with witness/timeout-refund covered by 5-test conformance suite. |
| `webycash-server-rgb-collectible` (RGB21) | **Ship.** Same HTLC machinery as RGB20; 5-test conformance suite (`server_rgb21_htlc.rs`) green. |
| `webycash-server-voucher` (issuer-namespaced bearer credits) | **Ship.** |
| `webylib` (Webcash + RGB client surface, HTLC `replace_with_htlc`) | **Ship.** End-to-end tests against compose stack green for RGB20 and RGB21. |
| `referee` orchestration **with mocks** (typestate + audit + push HMAC) | **Ship for staging.** 25 unit + 7 e2e tests green; production refuses to boot without `REFEREE_ALLOW_MOCK_CRYPTO=1`. |

The recommended first deployment slice is **webcash + RGB20 + RGB21 +
voucher** without the referee. That covers every flow except the
Webcash↔ARK swap and is fully proven against the conformance suite.

## What is gated on real-crypto integration

The referee design is complete — typestate, audit log, MuSig2 ceremony,
ZKP verifier interface, push webhook contract, hook contract for
extro-node — and every piece is exercised by mock-based integration
tests. What is **not** in this repo is the binding to real Groth16 and
real MuSig2 production code. The relevant files are gated by cargo
features and currently `unimplemented!()` so the binary refuses to
silently degrade:

| Gate | What still needs to land | Where |
|---|---|---|
| `--features zkp-arkworks` | Verifying-key bytes for the two BN254 circuits (Bob's payload-honesty, Alice's signature-honesty), produced by extro-node's circuit fixtures and bundled into the binary | `referee/src/main.rs::build_verifier`, `referee/docs/zkp-circuits.md` |
| `--features musig2-real` | Wiring the `musig2` crate's session types into `Musig2Signer`; loading the secp256k1 keypair from KMS at boot | `referee/src/main.rs::build_musig_signer`, `referee/docs/musig2-ceremony.md` |
| extro-node | Wallet implementation of `insert_hook` / `invalidate_hook` per `referee/docs/hook-contract.md` and the matching circuit definitions | external repo |

Until those land, the referee runs only with mocks and only with
`REFEREE_ALLOW_MOCK_CRYPTO=1` set in env. Booting without the flag
fails fast with a diagnostic naming the missing feature.

## Sequenced milestones

The codebase already delivers M-1 through M-3 and M-5 from the original
plan; M-4 (Groth16 toolchain validation) and the integration steps
remain.

### M-1 — Doc rename + cargo feature gating ✅
- `docs/atomic-swaps.md` renamed to `docs/referee-zkp-based-swap.md`;
  every flow now has its specific name.
- Workspace default-members: webcash only. Other rails opt-in by
  `cargo build -p <name>`.
- Banned-term sweep clean.

### M-2 — BIP32 + HTLC on RGB21 ✅
- `impl ReplaceHook for RgbCollectible` filled with HTLC machinery
  mirroring `RgbFungible`.
- `crates/conformance/tests/server_rgb21_htlc.rs` — 5 tests green.
- `webylib/tests/htlc_rgb21_e2e.rs` — 3 wallet-side tests green.
- BIP32 derivation work tracked in webylib (`hd/src/bip32.rs`); not on
  the server's critical path.

### M-3 — Webhook + hook contract specifications ✅
- `referee/docs/push-notification.md` — full webhook contract with
  HMAC-SHA256 signing, retry rules, and JSON shapes.
- `referee/docs/hook-contract.md` — `insert_hook` / `invalidate_hook`
  semantics for extro-node implementors.
- The hooks themselves live in extro-node, not webylib (per locked
  scope decision: webylib is non-custodial and never sees PGP private
  keys).

### M-4 — Groth16 toolchain validation 🟡 (in progress)
- Circuit specs documented in `referee/docs/zkp-circuits.md`.
- BN254 chosen; arkworks 0.5 toolchain documented.
- **Remaining**: pull verifying keys from extro-node's circuit
  fixtures, bundle them with the referee binary, switch
  `build_verifier` from `unimplemented!()` to `ArkworksVerifier::new(VK_BOB, VK_ALICE)`.

### M-5 — Referee binary ✅
- Full Axum service with feature-gated stubs for production crypto.
- Typestate transitions (`SwapInit → ZkpsVerified → PreChecked →
  InsertPushed → Settled|Aborted → Invalidated → Refunded`) — 25 unit
  tests.
- 7 mock-based e2e tests (settlement, abort, ZKP rejection, pre-check
  rejection, audit chain integrity, store reflects terminal phase).
- Async orchestration: `/v1/swap/initiate` returns swap_id immediately
  and `tokio::spawn`s the post-check loop in the background.
- Audited HMAC (`hmac` crate) for push webhook signing.
- Refuses to boot with mocks unless `REFEREE_ALLOW_MOCK_CRYPTO=1` is
  set.

### M-6 — extro-node wallet integration 🔲 (out of this repo)
- Wallet-side `insert_hook` / `invalidate_hook` implementation.
- ZKP prover for both circuits (Groth16/BN254).
- MuSig2 partial-sig generation on the wallet side.

This work happens in the extro-node repo; webycash-server only
specifies the contract that extro-node fulfils.

### M-7 — Referee real-crypto wiring 🔲
- Land `build_verifier` and `build_musig_signer` real bodies.
- Add a Postgres-backed `SwapStore` and `AuditLog` (currently
  in-memory only).
- Run a full e2e against:
  - real webcash.org test endpoints,
  - a real `webycash-server-rgb` instance,
  - extro-node wallet pair acting as Alice + Bob,
  - a synthetic ARK ASP for the vtxo side.

This is the production gate for the referee path.

### M-8 — Slashing bond / receipt-bound state machine 🔲 (future)
- Receipts cryptographically bind the abort path so a malicious
  referee cannot drop a refund silently.
- Slashing bond (deposit on the referee, slashed if it deviates from
  the audit log) — out of scope for v1.

## Pre-production checklist (per service)

Before flipping a hostname to production:

### Asset binaries (webcash, rgb-fungible, rgb-collectible, voucher)
- [ ] Conformance suite green on the deploy commit
- [ ] Backing store provisioned + backed up (Postgres or Redis)
- [ ] TLS at the proxy with HSTS + cert auto-renew
- [ ] Rate-limit at the proxy (10 req/s per IP default)
- [ ] Metrics exporter scraped by the deployer's monitoring stack
- [ ] Synthetic round-trip (mint + replace) succeeds against the deployed instance

### Referee
- [ ] Built with `--features zkp-arkworks,musig2-real,postgres`
- [ ] `REFEREE_ALLOW_MOCK_CRYPTO` **unset** in env
- [ ] Identity Ed25519 key, MuSig2 secp256k1 key, push HMAC key all from KMS
- [ ] Postgres audit table exists and is on a backed-up cluster
- [ ] Push provider configured; webhook signature verified end-to-end
- [ ] `REFEREE_INSERT_PUSH_RETRY` and `REFEREE_RETRY_BACKOFF_MS` reviewed against expected push-delivery latencies (default `3 × 250ms` is dev-only)
- [ ] Synthetic swap with test wallets settles and refunds successfully

## Known testing gaps

- **`start_swap` error-path coverage.** The orchestrator e2e tests
  drive `Orchestrator::run_swap` directly to assert that
  `RefereeError::ZkpRejected` / `InvalidTransition` bubble up. The
  HTTP path (`start_swap`) catches these in the spawned task and only
  logs them; a client observes "phase didn't progress" and inspects
  `/v1/swap/{id}/audit` for the failure detail. This behaviour is
  documented in `referee/docs/api.md` but not yet exercised by a
  dedicated test that uses `start_swap` + polling + audit-log read.
  Closing this gap belongs to M-7 (real-crypto integration), where the
  test will run against a real wallet pair anyway.

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| Push webhook silently drops a payload | medium | HMAC + receipt-ack at `/v1/swap/{id}/ack`; client-side polling fallback; bounded retry budget |
| Webcash.org `/health_check` flakes during post-check loop | medium | Bounded `REFEREE_INSERT_PUSH_RETRY`; abort path triggers refund |
| Referee crashes mid-MuSig2 | low | `referee_secret_nonces` is `UNLOGGED`; in-flight swap aborts cleanly on restart |
| Wallet implementor (extro-node) leaks PGP private key | high impact, low likelihood | Library never holds PGP keys; hook contract pins responsibility to wallet; deployer's threat model includes wallet OPSEC |
| Real Groth16 verifier rejects valid proofs (toolchain mismatch) | medium during integration | Conformance fixtures from extro-node must match the verifying keys bundled in the referee binary; test vector regression prevents drift |

## What's explicitly NOT on the roadmap

- **Webcash↔Voucher swap** — vouchers are bought directly from the
  issuer; permanently out of scope.
- **Custodial flows** — the library is non-custodial by construction;
  any custodial product belongs in a separate codebase.
- **Push notification service implementation** — operational concern
  of the deployer; we ship a webhook contract, not a service.
- **ARK ASP integration** — wallet-side concern (extro-node).
- **Hardware-bound key storage on wallets** — explicitly rejected.
