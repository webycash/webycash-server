# Referee production coordination + Cloudflare-adapter design notes

Status: coordination document. No implementation in this repository this phase.
No protocol change. This file fixes the boundary between the **production
referee** (a separate repo) and the **reference referee** (the `referee/` crate
in this repo), states the asset-server preservation guarantee, records
Cloudflare-adapter design notes for the *asset servers only*, and logs the
conflicts between older planning prose and the corrected architecture.

Read alongside:

- `docs/development-plan-cloudflare-rgb-adapters.md` (this repo's coordination
  plan).
- `docs/pair-policy.md` (the cross-rail pair matrix).
- `docs/referee-zkp-based-swap.md` (the swap protocol; §4 = referee flow).
- `extro/docs/development/24-extro-402-scheme-and-webycash-plan.md` (cross-repo
  roles + asset model).
- `referee/docs/README.md` (the reference crate's own docs; now banner-marked
  reference-only).

## §1. Two referees, one protocol

The swap protocol in `docs/referee-zkp-based-swap.md` is implemented by **two
distinct artifacts**, and this repository owns only the reference one:

| Artifact | Repo | Role | Status |
|---|---|---|---|
| **Production referee** | `webycash/webycash-referee` (`git@github.com:webycash/webycash-referee.git`) | The deployed mediator for **Webcash/Voucher ↔ Bitcoin ARK** swaps | Production target — built there, not here |
| **Reference referee** | `webycash-server/referee/` (this repo) | Protocol **reference + test oracle**: typestate, audit-chain shape, push/ZKP/MuSig2/client trait interfaces, idempotent advance loop | Reference only — not the production backend |

All production referee work goes to `webycash-referee`. The `referee/` crate
here is kept as a design reference and behavioral oracle; it is **not**
re-activated as a production service unless the user explicitly asks.

## §2. Production referee architecture (correction — authoritative)

Older cross-repo planning prose (see §6 conflict log) described the production
referee as a "Cloudflare Worker" using "Durable Objects." **That framing is
superseded.** The production `webycash-referee` is:

- **Native Rust, `hyper` + REST** — a serverless function (the
  Lambda-equivalent runtime model), **not** a WASM/V8 Cloudflare Worker and
  **not** TypeScript. This is the *same* runtime model the reference `referee/`
  crate already targets: its source is written for a stateless serverless
  process — see `referee/src/musig2.rs` ("a stateless Lambda restart aborts
  in-flight swaps and the refund path engages"), `referee/src/store/mod.rs` and
  `referee/src/api/orchestrator.rs` (the orchestrator holds no in-process state
  between invocations).
- **Authoritative state in a transactional / conditional-write store** —
  DynamoDB with `ConditionExpression`, or FoundationDB serializable
  transactions — to enforce **settle-XOR-refund** atomically. The reference
  crate already ships exactly these backends:
  `referee/src/store/{dynamodb,fdb}.rs` and
  `referee/src/audit/{dynamodb,fdb}.rs`. This conditional-write store is the
  mechanism that prevents a swap from both settling and refunding; **Cloudflare
  Durable Objects are NOT used** for it.
- **Durable Objects: N/A to the referee.** Durable Objects are a
  Workers-runtime-only primitive; a native `hyper` process cannot bind them.
  The serialized settle-XOR-refund guarantee is provided by the transactional
  store above, not by a Durable Object.
- **Cloudflare KV / R2, if used at all, only via their REST APIs** — for
  secondary indexes or archival blobs that a native process reaches over HTTPS
  — never as the authoritative settle/refund state.

### Note on the HTTP library

The runtime-model property that matters is **native Rust serverless + stateless
restart + transactional store** versus **CF Worker / V8 / TypeScript /
Durable Objects** — *not* the specific HTTP crate. The reference `referee/`
crate wires its routes with `axum` (`referee/Cargo.toml`); that is correct for
the reference and is **not** a defect to "fix." The production repo states
`hyper` + REST; both are native-Rust serverless and share the stateless +
transactional-store model the correction asserts.

## §3. Reference / test-oracle status of the `referee/` crate

The `referee/` crate stays in the workspace as the protocol reference and the
behavioral oracle for `webycash-referee`. `referee/docs/README.md` now carries a
top banner marking the whole `referee/docs/` set REFERENCE / TEST-ORACLE.

What `webycash-referee` should **reuse as concepts** (not copy blindly), per
`docs/development-plan-cloudflare-rgb-adapters.md`:

- typestate phases (`SwapInit → ZkpsVerified → PreChecked → InsertPushed →
  Settled | Aborted → Invalidated → Refunded`);
- append-only signed audit-chain shape;
- push request kinds (`insert` / `invalidate` / `release-settle` /
  `release-refund`);
- the `Verifier` (Groth16) and `Musig2Signer` trait interfaces;
- the `WebcashClient` / `RgbClient` abstraction;
- the idempotent advance loop (`/v1/swap/{id}/advance`, one post-check
  iteration per invocation — the stateless-friendly entry point).

What `webycash-referee` **replaces**:

- the reference HTTP runtime with the production `hyper` + REST serverless
  function;
- in-memory / mock stores with the transactional store
  (DynamoDB `ConditionExpression` / FoundationDB);
- generic HMAC webhook push with the Extro keyserver push dispatch;
- mock Webcash / RGB clients with real clients (or production test doubles);
- mock crypto with real Groth16 verification and real MuSig2 co-signing.

Note (out of scope to fix this phase): the reference crate's own prose docs
(`referee/docs/architecture.md`, `deployment.md`, `trust-model.md`) describe an
`axum` + Postgres + nginx deployment, which drifts from the crate's actual
stateless-serverless + DynamoDB/FDB/Redis source. Those bodies are left
untouched; this coordination doc is the authoritative description of the
runtime + storage model, and the README banner points readers here.

## §4. Asset-server behavior is preserved

This phase changes **no asset-server behavior**. The existing
Webcash / RGB20 / RGB21 / Voucher ledger servers, the `asset-core` trait
hierarchy, the `ReplaceHook` contract, and the RGB HTLC predicate all keep
their current semantics:

- Webcash default = bearer-only, no RGB, no Voucher, no referee
  (`referee-zkp-based-swap.md` §11); other rails opt-in by cargo feature /
  per-binary build.
- Webcash & Voucher `/replace` stays the no-op-`ReplaceHook` single-use-seal
  closure; they remain non-conditional.
- RGB keeps its AluVM HTLC predicate gate and server-clock lock-time discipline.
- The conformance suite (Webcash lifecycle, Voucher issue/replace, RGB20 &
  RGB21 HTLC lock/claim/refund) remains the behavioral contract.

## §5. Cloudflare adapter — DESIGN NOTES ONLY (do not implement)

These notes are deliberately scoped to the **asset-server storage layer**, not
the referee, and are **not** implemented or extended this phase (the CF adapter
is explicitly skipped). They exist so a later, separately-audited adapter
session starts from the right constraints. Per
`docs/development-plan-cloudflare-rgb-adapters.md`, adapter work targets the
asset server layer; the referee's storage is settled by §2 above (transactional
store, not CF primitives).

| CF primitive | Candidate role (asset server) | Risk / gate |
|---|---|---|
| **KV** | Token-state and public-hash *lookup/index* where eventual consistency is tolerable | **Eventual-consistency risk.** KV is last-writer-wins with propagation lag; it must **not** hold authoritative replace/burn state unless the exact write pattern is proven safe under KV semantics (guarded by strong idempotency or external ordering). Do **not** move RGB replace state to KV-only. |
| **D1** | **Transactional candidate** for authoritative replace / burn / issue state when strict serial order is required | SQLite-backed; gives real transactions. The leading candidate for the conservation-critical write path. |
| **R2** | **Immutable artifacts** — audit blobs, proof blobs, large records | Append-only / write-once usage; never the live conservation state. |
| **Durable Objects** | Per-contract / per-namespace serialized writes — *for the asset server* | A design-note candidate **only**; not implemented. Distinct from the referee, where DO is N/A (§2). Any use requires the storage-consistency audit first. |

Hard security gate (carried verbatim from the coordination plan and the
extro-402 invariants):

- **Do not move RGB replace state to KV-only** unless the exact replace
  operation is *proven* safe under KV consistency semantics.
- KV alone must not be assumed to provide serializable state transitions until
  a Cloudflare storage audit proves the specific write pattern is safe.
- Implement an adapter only **after** that storage-consistency audit — not this
  phase.

## §6. Conflict log (older prose vs corrected architecture)

Recorded so downstream agents do not "rediscover" these as bugs. The correction
in §2 is authoritative; the entries below are the prior framings it supersedes.

1. **Production referee runtime.** `extro/docs/development/24-...md` (repo-roles
   §) and `docs/development-plan-cloudflare-rgb-adapters.md` ("Cloudflare
   adapter direction" / "Referee reference extraction") describe the production
   referee as a **Cloudflare Worker**. **Corrected:** production
   `webycash-referee` is a **native Rust `hyper` + REST serverless function**
   (§2). CF Workers are a different runtime; this referee does not run on them.

2. **Settle-XOR-refund mechanism.** Older planning prose and the reference
   spec's adapter list (`docs/development-plan-cloudflare-rgb-adapters.md`)
   imply **Durable Objects** for serialized per-swap writes. **Corrected:** the
   referee enforces settle-XOR-refund with a **transactional / conditional-write
   store** (DynamoDB `ConditionExpression` / FoundationDB serializable
   transactions). Durable Objects are Workers-only and unavailable to a native
   `hyper` process. (Durable Objects remain a *candidate design note for the
   asset server* per §5 — a separate component.)

3. **Reference vs production status.** The `referee/` crate's own docs and the
   repo roadmap describe a deployable Axum/Postgres referee service.
   **Clarified:** that crate is **reference / test oracle only**; the production
   backend is `webycash-referee` (§1, §3). The crate's internal Axum/Postgres
   vs stateless-serverless/DynamoDB-FDB drift is noted but left unedited this
   phase (§3 note).

None of the above is a blocker: the corrected architecture in §2 is the agreed
direction this coordination work records, and the asset servers (§4) are
unaffected.
