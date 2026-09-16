# Agent Prompt: Webycash Server Coordination and Reference

## Prompt

You are updating coordination docs and reference tests in
`/Users/george/workspace/webycash/webycash-server`.

Read first:

- `docs/development-plan-cloudflare-rgb-adapters.md`
- `docs/referee-zkp-based-swap.md`
- `referee/docs/*`
- `crates/asset-core/src/lib.rs`
- `crates/asset-rgb/src/htlc/*`
- conformance tests for Webcash, Voucher, RGB20, and RGB21.

Branch:

```text
feature/server-referee-reference-and-cloudflare-adapter-plan
```

Create the branch only after the coordinator/user authorizes implementation.

## Objective

Keep `webycash-server` aligned with the new architecture:

- existing Rust referee is reference/test oracle;
- production Cloudflare referee lives in `webycash/webycash-referee`;
- Webcash/Voucher stay bearer-only;
- RGB keeps conditional/HTLC behavior;
- future Cloudflare adapters are planned carefully.

## Implementation Request

1. Update docs to point production referee work to `webycash-referee`.
2. Update old Rust referee docs so they are clearly marked reference-only.
3. Add or update pair-policy documentation:
   - Webcash <-> Voucher forbidden;
   - bearer <-> ARK referee;
   - RGB conditional flows;
   - RGB <-> RGB no-referee where server can enforce conditions.
4. Add Cloudflare adapter design notes for asset servers:
   - KV risks;
   - D1 transactional candidate;
   - R2 immutable artifacts;
   - Durable Objects for serialized per-namespace/per-contract writes.
5. Preserve existing asset server behavior.

## Optional Reference Tests

If the user asks for code/tests, add tests that document reference behavior
without changing protocols:

- Webcash/Voucher `ReplaceHook` is no-op conditional hook.
- RGB HTLC rejects missing witness.
- RGB HTLC accepts claim with correct preimage.
- RGB HTLC accepts refund after timeout.
- Webcash/Voucher cannot be made conditionally locked by request extras.

## Commands To Run

Run from `webycash/webycash-server`:

```bash
cargo fmt
cargo test -p webycash-server-conformance
cargo test --workspace
```

If Docker-backed conformance tests skip due to Docker absence, report that
clearly. If full workspace is too slow, run targeted conformance tests and
explain.

## Deliverables

Return:

- docs/tests changed;
- exact behavior clarified;
- test commands and results;
- any conflicts between old referee docs and new Cloudflare plan.

## Do Not Do

- Do not implement the Cloudflare referee in this repo.
- Do not change Webcash/Voucher into conditional assets.
- Do not add centralized orderbook matching.
- Do not remove the Rust referee reference unless the user asks.
