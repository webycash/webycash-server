# Local server-side stack (Phase D)

Everything server-side running locally as its native self against a faithful
Cloudflare emulation, plus an end-to-end smoke test that proves the C3 bearer
rail against the **live** webcash server.

| Service | Port | What it is | Storage (Cloudflare-faithful) |
|---|---|---|---|
| `server-webcash` | 8181 | native `webycash-server-webcash` | redis ledger (= production redis path) |
| `server-rgb` | 8182 | native `webycash-server-rgb` | redis |
| `server-voucher` | 8183 | native `webycash-server-voucher` | redis |
| `extro-keyserver` | 7800 | native `extro-keyserver` (default build) | in-memory identity store, dev operator key |
| `webycash-referee` | 8090 | native `webycash-referee` | **SQLite file = D1** + real `HttpBearerRail` → 8181 |

`D1 IS SQLite`: the referee opens a local `.sqlite` file directly
(`WEBYCASH_REFEREE_DB_PATH`) — the exact store/audit code path that runs on
Cloudflare D1 via the worker-proxy. No D1 container exists or is needed.

## Why native binaries, not docker-built

The Rust servers are built with `cargo build --release` as **host** binaries and
launched directly. They are NOT `docker build`-ed: a docker build runs cargo
inside a container the host OOM ps-guard cannot see, which would allow two
concurrent cargo builds and crash the machine. `docker-compose.local.yml` here
is **infrastructure only** (prebuilt off-the-shelf images: redis, and the
profile-gated minio/dynamodb/bitcoind/esplora) — it has no `build:` stanza.

## Cold start

```bash
# 1. Build the native binaries (one cargo at a time — check the OOM ps-guard:
#    `ps -axo ucomm= | grep -cx cargo` and `… grep -cx rustc` must both be 0):
cd ../                       # webycash-server
cargo build --release -p webycash-server-webcash -p webycash-server-rgb -p webycash-server-voucher
cd ../../extro/extro-keyserver && cargo build --release --bin extro-keyserver
cd ../../webycash/webycash-referee \
  && cargo build --release --features zkp-arkworks,musig2-real \
  && cargo build --release --example rail_probe --features zkp-arkworks,musig2-real

# 2. Bring the stack up (starts redis + launches the native binaries):
cd ../../webycash/webycash-server/local-stack
./run-local.sh up

# 3. Smoke test (server-side proof, incl. the live C3 rail round-trip):
./smoke-test.sh

# 4. Inspect / tear down:
./run-local.sh status
./run-local.sh down
```

Dev key material (Ed25519 operator seed, 40-hex fingerprint, secp256k1 MuSig2
secret) is generated 0600 under `.run/` on first `up` — deterministic,
**never** for production.

## What the smoke test asserts (last verified run)

```
== (a) liveness ==
  PASS: webcash /target
  PASS: keyserver /health
  PASS: keyserver pubkey surface (/.well-known/extro-keyserver)
  PASS: referee /v1/health
== (c) referee SQLite (D1) store + pubkey ==
  PASS: referee /v1/pubkey: {"ed25519_pubkey_hex":"3b6a27bc…","musig2_pubshare_hex":"02466d7f…","referee_version":"0.1.0"}
  PASS: referee booted on SQLite (D1-faithful) store
== (b) mine a webcash, then C3 rail round-trip against the LIVE server ==
  PASS: mined preimage (nonce=6)
  PASS: mining_report accepted
  C3 real HttpBearerRail (pre-spend): Unspent
  PASS: REAL HttpBearerRail reads Unspent (live server, public-webcash key, not bare H)
  raw server health_check (pre-spend): {"status":"success","results":{"e1:public:b4998aa8…":{"spent":false,"amount":"1"}}}
  PASS: server keyed results by the normalized public-webcash string e1:public:H
  PASS: replace spent the webcash
  C3 real HttpBearerRail (post-spend): Spent
  PASS: REAL HttpBearerRail reads Spent after /replace (C3 settlement-authority transition proven end-to-end)
```

The C3 proof: the **real** referee `HttpBearerRail` (referee `examples/rail_probe`,
the exact client wired into the running referee binary) submits the
public-webcash string `e1.0:public:{H}` to the live `/api/v1/health_check`,
which keys its results by the normalized `e1:public:{H}` and returns
`spent:false` (→ `Unspent`) then, after `/replace`, `spent:true` (→ `Spent`).
A bare 64-hex `H` (the pre-C3 behaviour) fails the server's `parse_public` and
returns non-2xx — never the silent `Unknown` an old comment claimed.

## ZKP verifier (Phase E note)

The referee boots with a **real** arkworks Groth16/BN254 verifier, not
fail-closed: `ArkworksVerifier::from_env_bearer()` defaults to
`extro/extro-circuits/circuits-dev/vk_bearer_payload.bin` when
`EXTRO_ZKP_VK_PATH` is unset, and that dev VK exists. `run-local.sh` wires the
path explicitly. The VK is not load-bearing for Phase D (the smoke test never
calls swap-initiate), but it is for the Phase E Alice↔Bob ZKP gate.

## ARK ASP — the one external blocker for full bitcoin-ARK (Phase E)

The server-side stack (webcash/rgb/voucher + keyserver + referee/D1 + the C3
rail) comes up and is proven **without** an ARK ASP. The bitcoin-ARK 2-of-2
coordinator (the ASP) is the single external dependency missing locally; it is
required only for the conditional (Bitcoin-ARK) leg of an Alice↔Bob swap, not
for any server-side surface above.

Options for the operator (pick one before Phase E's ARK leg):

1. **arkade testnet account** — point the referee's ARK signer adapter at a
   real testnet ASP. Most faithful; needs an external account/credentials.
2. **Minimal mock ASP** — a tiny local 2-of-2 settle/refund coordinator behind
   the existing `ArkSigner` trait (`src/rails/mod.rs` already has `MockArkSigner`).
   No external account; lets the Alice↔Bob state machine run end-to-end locally
   with a stand-in for ASP broadcast/finalization.

`docker-compose.local.yml --profile ark` provides `bitcoind` (regtest) as the
base chain layer an ASP would sit on; `esplora` is stubbed (it needs bespoke
regtest wiring — see the compose) and should not be relied on until configured.

## Keyserver persistence (faithful path, optional)

The default keyserver build used here keeps identity state **in memory** (the
smoke test only exercises `/health` + the discovery/pubkey surface, neither of
which persists). There is no "sled" backend — the keyserver is
DynamoDB-or-in-memory. For the faithful persistent path, build with the
`keyserver-dynamo` feature and bring up the `dynamo` + `r2` profiles
(`dynamodb` + `minio` with the `extro-backend-dev-backup` bucket); this is
offered as an operator option exactly like the ASP, since no Phase-D smoke
assertion requires it.
