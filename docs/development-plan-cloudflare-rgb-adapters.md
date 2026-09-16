# Development Plan: Webycash Server Coordination

Status: planning document, no implementation started.

This repository continues to own Webcash/RGB/Voucher ledger servers and asset
traits. It does not become the final Webycash referee. The existing Rust
`referee` crate remains a reference implementation and test oracle for the new
Cloudflare Worker repository `webycash-referee`.

## Goals

- Keep Webcash/RGB/Voucher server behavior stable.
- Preserve RGB HTLC/AluVM predicate behavior for conditional asset flows.
- Prepare future Cloudflare storage adapters for RGB server state.
- Keep old Rust referee docs useful as protocol reference while moving the
  production referee to the new `webycash-referee` repo.

## Non-Goals

- Do not build the Cloudflare referee in this repo.
- Do not add Webycash DEX UI.
- Do not add centralized orderbook matching.
- Do not extend Webcash/Voucher servers with fake conditional semantics.

## Existing Useful Components

- `asset-core::ReplaceHook` already models the important distinction:
  Webcash and Voucher are no-op conditional hooks, RGB overrides with HTLC
  predicate behavior.
- `asset-rgb::htlc` already provides a server-side predicate gate for locked
  RGB outputs.
- Conformance tests cover RGB20 and RGB21 HTLC flows.
- The Rust `referee` crate has useful typestate, audit, push, and ZKP verifier
  reference code.

## Required Documentation Updates

Before implementation agents touch code, update docs to clarify:

- `webycash-referee` is the production Cloudflare Worker target.
- Existing `webycash-server/referee` is reference-only unless explicitly
  reactivated.
- Webcash/Voucher remain bearer-only.
- RGB server can help with RGB <-> RGB and RGB <-> conditional rail flows.
- Webcash <-> Voucher remains forbidden.

## Cloudflare Adapter Direction

Later adapter work should target the asset server layer, not the referee:

- KV candidate: token state and public hash lookup where eventual consistency is
  acceptable only if writes are guarded by strong idempotency or external
  ordering.
- D1 candidate: transactional replace/burn/issue state if strict serial order is
  needed.
- R2 candidate: audit blobs, proof blobs, large immutable artifacts.
- Durable Objects candidate: per-contract or per-namespace serialized updates.

Security gate:

- Do not move RGB replace state to KV-only unless the exact replace operation
  can be proven safe under KV consistency semantics.

## Referee Reference Extraction

The Cloudflare repo should reuse concepts, not copy blindly:

- typestate phases;
- audit chain shape;
- push request kinds;
- ZKP verifier interface;
- MuSig2 signer interface;
- Webcash/RGB client abstraction;
- idempotent advance loop.

But it should replace:

- Axum runtime with Cloudflare Worker runtime;
- Redis/DynamoDB/FDB stores with Cloudflare storage;
- generic HMAC webhook push with Extro keyserver push dispatch;
- in-memory/mock webcash and RGB clients with real clients or test doubles.

## Tests To Preserve

- Webcash full lifecycle.
- Voucher issue and replace lifecycle.
- RGB20 HTLC lock/claim/refund.
- RGB21 HTLC lock/claim/refund.
- Storage key namespace isolation.
- Pair policy tests should live in Extro/PWA/referee repos, not asset server
  core unless they validate server-specific behavior.

## Implementation Order

1. Add this coordination plan.
2. Update referee docs to point to `webycash-referee` as production target.
3. Keep Rust referee tests as reference until Cloudflare equivalent exists.
4. Design RGB Cloudflare adapter separately.
5. Implement adapter only after storage-consistency audit.

## Agent Prompt

Use `docs/agent-prompts/01-server-coordination-and-reference.md` for the
implementation agent assigned to this repository.
