# Alice↔Bob swap round-trip — runbook (signet dry-run → tiny mainnet)

This runbook is the operator's guide to running the webcash↔Bitcoin-ARK swap
round-trip: first a **signet** dry run (valueless test network), then — and only
then, and only by the operator — a **tiny mainnet** run.

> **SAFETY.** `swap-roundtrip.sh` defaults to **signet** and **refuses mainnet
> non-interactively**. It never broadcasts or moves funds itself. The live ARK
> 2-of-2 VTXO settlement is performed by the operator's wallet (extro-node
> `ArkHandle`) against a real ASP — this runbook tells you exactly where that
> step is and how to do it. Never point the referee or a wallet at a mainnet ASP
> while testing.

## Architecture recap (who does what)

- **Referee** (`webycash-referee`): co-signs **one half** of the ARK 2-of-2
  MuSig2 output and adjudicates release/refund. Per MuSig2 invariant 2 it
  **never** holds a complete signature, **never** broadcasts, **never** enters
  an ASP round. Its release decision is gated on the **bearer-rail spent-status
  post-check**, not on the ASP. Its only ASP work is **read-only VTXO
  validation** via the `ark-asp` `RealArkSigner` (config: `ARK_ASP_URL`,
  `ARK_NETWORK`; default signet; mainnet explicit).
- **Wallet** (extro-node `ArkHandle`, real `ark-client` 0.8): Alice receives the
  referee's partial + Bob's encrypted partial, **decrypts, aggregates, and runs
  the ASP round** to settle the VTXO. **This is the live ARK leg** — the
  operator's, against the operator's ASP credentials.
- **Webcash server**: the bearer leg's closure server. The seller's webcash `H`
  is minted/observed here; `/replace` consumes it (recipient takes ownership);
  `health_check` returns `{spent, amount}` — the settlement authority.

## What `swap-roundtrip.sh` proves locally, and the one gap

Running `./swap-roundtrip.sh` against `./run-local.sh up` drives, **live over
HTTP** for each of the two round-trip legs:

1. referee `/v1/health` + `/v1/pubkey`;
2. a **real two-proof** `InitiateRequest` built by extro-node's
   `swap_initiate_request` example (real Groth16/BN254 bearer proof);
3. the webcash bearer leg **mined on the live server** (so `H` exists, unspent);
4. the referee's **real `HttpBearerRail`** spent-status **pre-check → post-check
   (Unspent → Spent)** transition — the settlement-authority observation;
5. a POST to the live `/v1/swap/initiate`.

**The one gap (ConditionalLeg / B2c).** The live referee runs the real arkworks
verifier and recomputes the **conditional** binding with `settlement_tx_hash =
Some` ⇒ **7 public inputs**. The `extro-circuits` dev ceremony fixes a
**6**-public-input `BearerPayloadCircuit`, so the conditional proof is
**rejected (422)**. The bearer proof verifies; the conditional one cannot until
a 7-input ceremony exists. This is pinned empirically by the referee test
`conditional_leg_real_proof_is_rejected_documents_b2c_gap`. **Closing it** =
adding a conditional circuit/ceremony (full in-circuit MuSig2 is the long-term
B2c target; an interim is a 7-input bearer-style ceremony bound to the
settlement tx). Until then the full live `/v1/swap/initiate` two-proof accept is
the remaining build step; everything else runs live.

## Signet dry run (do this first)

### 1. Bring up the local server-side stack

```sh
cd webycash/webycash-server/local-stack
./run-local.sh up          # webcash :8181, keyserver :7800, referee :8090 (SQLite=D1)
./smoke-test.sh            # confirms the C3 rail round-trip against the live server
```

### 2. Build the two example binaries (ONE cargo at a time — OOM ps-guard)

```sh
# Wallet-side real-proof InitiateRequest builder:
cd extro/extro-node
cargo build --release --example swap_initiate_request --features scheme402
#   then point the script at the release build, or build debug (slower proving):
#   cargo build --example swap_initiate_request --features scheme402

# The referee's real HttpBearerRail probe (already built by run-local.sh's deps):
cd webycash/webycash-referee
cargo build --release --example rail_probe --features zkp-arkworks,musig2-real
```

### 3. Run the round-trip driver (signet default)

```sh
cd webycash/webycash-server/local-stack
./swap-roundtrip.sh                       # network=signet (default), TINY amount
# or explicitly:
EXTRO_SWAP_NETWORK=signet ./swap-roundtrip.sh
```

Expect: every step PASS except the live `/v1/swap/initiate`, which reports the
B2c gap (422 on the conditional proof) and then still proves the rail
post-check transition.

### 4. Fund + run the live ARK leg (signet)

The ARK leg is not driven by the script — it is the wallet's. On signet:

```sh
# Connect a signet ARK wallet (extro-node), board sats, then send a VTXO.
# (extro-node CLI; the exact subcommands are under `extro-node ark …`.)
#   - get a signet faucet payout to your on-chain boarding address:
#       extro-node ark board            # prints the boarding address; fund it from
#                                       #   a signet faucet (e.g. https://signetfaucet.com)
#   - once boarded, the wallet has a signet VTXO it can lock into the 2-of-2.
```

Point the referee's read-only ASP validation at your signet ASP so it can
confirm the locked VTXO (read-only — never broadcasts):

```sh
# Rebuild the referee WITH the ark-asp feature, then set the env:
cd webycash/webycash-referee
cargo build --release --features zkp-arkworks,musig2-real,ark-asp
ARK_ASP_URL=https://<your-signet-asp>  ARK_NETWORK=signet  \
  WEBYCASH_REFEREE_RAIL_BASE_URL=http://127.0.0.1:8181  \
  WEBYCASH_REFEREE_MUSIG2_KEY_PATH=.run/musig2_secret.hex  \
  WEBYCASH_REFEREE_DB_PATH=.run/referee.sqlite  \
  target/release/webycash-referee
```

The full signet swap then is: build the conditional 2-of-2 with your signet
VTXO + the referee's MuSig2 pubshare (`/v1/pubkey`), initiate (once the 7-input
conditional ceremony lands), let the referee co-sign, and have the wallet
aggregate + settle the VTXO against the signet ASP. The webcash bearer leg is
already exercised by the script.

## Tiny mainnet run (operator only, after a clean signet dry run)

> `swap-roundtrip.sh` will **refuse** to drive mainnet. These steps are manual.

1. Do a **complete, clean signet dry run** first (above). Do not proceed
   otherwise.
2. Use a **tiny** amount (dust-level VTXO + a 1-unit webcash) — the smallest the
   ASP and the webcash server accept.
3. Run the referee with `ARK_NETWORK=mainnet` **explicitly** (the `RealArkSigner`
   logs a loud mainnet warning and still only does read-only validation):
   ```sh
   cargo build --release --features zkp-arkworks,musig2-real,ark-asp
   ARK_ASP_URL=https://<mainnet-asp>  ARK_NETWORK=mainnet  …  target/release/webycash-referee
   ```
4. Point the webcash rail at the **real** webcash server
   (`WEBYCASH_REFEREE_RAIL_BASE_URL=https://webcash.org` or your production
   server) — the bearer leg uses real webcash.
5. Build the conditional 2-of-2 with a **mainnet** ARK VTXO from your wallet +
   the referee's MuSig2 pubshare.
6. Initiate (after the conditional ceremony lands), let the referee co-sign,
   then **your wallet** aggregates the partials and settles the VTXO against the
   **mainnet** ASP. The referee never broadcasts; you do.
7. Verify on both rails: webcash `health_check` shows the bearer `H` spent; the
   ASP shows the VTXO settled to the counterparty.

## Remaining gaps before a fully-live mainnet swap

| Gap | Owner | Note |
|---|---|---|
| Conditional ZKP (B2c) — 7-input ceremony / in-circuit MuSig2 | build | live `/v1/swap/initiate` 422s on the conditional proof until this lands; bearer proof already verifies |
| Live ARK ASP round (settle the VTXO) | operator | extro-node `ArkHandle` ↔ a real ASP; no ASP reachable in CI |
| Mainnet trusted-setup ceremony + external audit | operator | before any real funds (per the morning report deploy phase) |
