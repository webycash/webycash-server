# Local integration setup

End-to-end test topology for the asset-gated server family.

## Topology

```
┌──────────────────────────┐       ┌────────────────────┐
│ webylib integration test │──────▶│ server-webcash :8181│──▶ redis-webcash
│  (cargo test)            │       └────────────────────┘
│                          │       ┌────────────────────┐
│                          │──────▶│ server-rgb     :8182│──▶ redis-rgb
│                          │       └────────────────────┘
│                          │       ┌────────────────────┐
│                          │──────▶│ server-voucher :8183│──▶ redis-voucher
└──────────────────────────┘       └────────────────────┘
```

## Quickstart

```bash
# From webycash-server/
docker compose -f docker-compose.local.yml up --build -d

# Wait for the three servers to be reachable
curl -fsS http://localhost:8181/api/v1/target
curl -fsS http://localhost:8182/api/v1/target
curl -fsS http://localhost:8183/api/v1/target

# From webylib/, run the cross-flavor lifecycle suite
cargo test --features docker-local -p webylib-conformance

# Tear down
cd ../webycash-server
docker compose -f docker-compose.local.yml down -v
```

## Lifecycle exercised per flavor

| Step       | Webcash             | RGB20 split.   | RGB21 NFT      | Voucher        |
|------------|---------------------|----------------|----------------|----------------|
| Mint/Issue | `/mining_report`    | `/issue`       | `/issue`       | `/issue`       |
| Balance    | `/health_check`     | same           | same           | same           |
| Split      | `/replace`          | `/replace`     | n/a (transfer) | `/replace`     |
| Transfer   | `/replace`          | `/replace`     | `/transfer`    | `/replace`     |
| Recover    | snapshot round-trip | same           | same           | same           |

Each step is a single test function; failures show which leg of which flavor
broke. Tests are gated by `--features docker-local` so they don't run on
machines without Docker.

## Status (2026-04-25, post-M0)

- M0 ships the topology, Dockerfile, and compose definition.
- Server binaries currently exit immediately printing `"M0 stub"`. M1 wires up
  `server-webcash` for real; M3 wires `server-rgb`; M5 wires `server-voucher`.
- The webylib-conformance crate ships the test harness skeleton; per-flavor
  tests light up as M2 / M4 / M6 land.

## Why a separate compose file from `docker-compose.yml`?

`docker-compose.yml` is the throughput benchmark setup (3-redis × 3-server
load test, plus FoundationDB + DynamoDB Local). Mixing it with the local
integration setup conflates two concerns. Bench stays bench; local stays
local. Both can be present in the same checkout without conflict.
