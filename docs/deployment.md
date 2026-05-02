# webycash-server — production deployment

How to deploy the full stack: the four asset-flavour servers
(`webcash`, `rgb-fungible`, `rgb-collectible`, `voucher`) plus the
`referee` swap helper, with their backing infrastructure.

For referee-only operational specifics see
[`referee/docs/deployment.md`](../referee/docs/deployment.md). For the
protocol that the referee mediates see
[`referee-zkp-based-swap.md`](referee-zkp-based-swap.md).

## Topology

```
                      ┌──────────────────┐
                      │    nginx / TLS   │
                      └─────────┬────────┘
        ┌────────────┬──────────┼─────────────┬──────────────┐
        ▼            ▼          ▼             ▼              ▼
  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌─────────────┐ ┌─────────┐
  │ webcash  │ │ rgb-fung │ │ rgb-coll │ │   voucher   │ │ referee │
  │  :8080   │ │  :8081   │ │  :8082   │ │    :8083    │ │  :8090  │
  └────┬─────┘ └────┬─────┘ └────┬─────┘ └──────┬──────┘ └────┬────┘
       │            │            │              │             │
       └────────────┴────────────┴──────────────┘             ▼
                            │                          (referee Postgres
                            ▼                           + KMS for keys
              (Redis / DynamoDB / FoundationDB,         + push provider)
               picked per-deployment via
               WEBCASH_DB_BACKEND)
```

Each asset binary is a self-contained service that owns its data layer.
The referee never reads asset stores directly — it talks to the RGB
server's HTTP API to mint the swap-tracking record, and to webcash.org's
public `/api/v1/health_check` for the spent-status check on the webcash
leg. There is **no** shared database between asset binaries and the
referee.

## Build matrix

```sh
# webcash only (default — no extra features)
cargo build --release -p webycash-server-webcash

# rgb-fungible + rgb-collectible (RGB20 + RGB21)
cargo build --release -p webycash-server-rgb
cargo build --release -p webycash-server-rgb-collectible

# voucher
cargo build --release -p webycash-server-voucher

# referee — production crypto features ON
cargo build --release -p referee \
    --features zkp-arkworks,musig2-real,postgres
```

Workspace default (`cargo build` with no `-p`) compiles only
`webycash-server-webcash`. Every other binary is opt-in. This keeps the
default build fast and means a webcash-only deployment never carries
RGB / voucher / referee code.

## Per-binary requirements

### Shared env vars (all four asset binaries)

The asset binaries share the same env-var convention. The exact
canonical list lives in each `crates/server-*/src/main.rs`; the names
below are the ones actually read at boot.

| Var | Default | Purpose |
|---|---|---|
| `WEBCASH_BIND_ADDR` | `0.0.0.0:8080` | `host:port` to bind |
| `WEBCASH_MODE` | `testnet` | `testnet` or `mainnet` |
| `WEBCASH_DB_BACKEND` | `redis` | One of `redis`, `dynamodb`, `foundationdb` |
| `REDIS_URL` | (unset) | Redis URL when `WEBCASH_DB_BACKEND=redis` |
| `DYNAMODB_ENDPOINT` | (unset) | DynamoDB endpoint when backend = `dynamodb` |
| `FDB_CLUSTER_FILE` | (unset) | FoundationDB cluster file when backend = `foundationdb` |
| `WEBYCASH_DIFFICULTY` | binary default | Mining difficulty override |
| `WEBYCASH_MINING_MODE` | enabled | Set to `disabled` to skip the mining loop |

Run different asset binaries on different ports by setting
`WEBCASH_BIND_ADDR` differently in each service's environment — the
binaries do not coordinate; each owns its own data layer.

### webycash-server-webcash

Wire-protocol-frozen against webcash.org. No extra env vars beyond the
shared list.

### webycash-server-rgb (RGB20) and webycash-server-rgb-collectible (RGB21)

Issuer-namespaced; AluVM-validated `replace`; HTLC primitive used by
the referee + RGB↔X HTLC swaps. RGB21 is non-splittable (1:1 transfer
with amount equality).

| Var | Purpose |
|---|---|
| `WEBYCASH_ISSUERS` | Comma-separated list of accepted issuer PGP fingerprints |
| `WEBYCASH_ISSUER_PGP_CERTS` | Path to a directory of PGP certificate files for those issuers |

### webycash-server-voucher

Always-splittable, issuer-namespaced. Same `WEBYCASH_ISSUERS` /
`WEBYCASH_ISSUER_PGP_CERTS` env vars as RGB.

### referee

See [`referee/docs/deployment.md`](../referee/docs/deployment.md). The
short version:

| Var | Purpose |
|---|---|
| `REFEREE_BIND` | `host:port` |
| `REFEREE_IDENTITY_KEY_PATH` | Ed25519 secret hex, 0600 |
| `REFEREE_RGB_SERVER_URL` | URL of `webycash-server-rgb` (swap-tracking record sink) |
| `REFEREE_WEBCASH_SERVER_URL` | `https://webcash.org` |
| `REFEREE_PUSH_WEBHOOK_URL` | Deployer's push-provider webhook |
| `REFEREE_PUSH_WEBHOOK_HMAC_KEY_PATH` | 32-byte HMAC key, 0600 |
| `REFEREE_POSTGRES_URL` | Optional — when `--features postgres` is built |
| `REFEREE_INSERT_PUSH_RETRY` | Default 3 |
| `REFEREE_RETRY_BACKOFF_MS` | Default 250 (ms between post-check attempts) |

The referee refuses to boot with mock crypto unless
`REFEREE_ALLOW_MOCK_CRYPTO=1` is set. Production should always build
with `--features zkp-arkworks,musig2-real`.

## Backing infrastructure

### Asset-binary storage (Redis / DynamoDB / FoundationDB)

All four asset binaries (`webcash`, `rgb`, `rgb-collectible`, `voucher`)
share the same pluggable storage layer; pick one backend per
deployment via `WEBCASH_DB_BACKEND`:

- `redis` — default. AWS ElastiCache or self-hosted cluster mode with
  TLS. Reservations are short-lived; a 1-hour AOF window is sufficient
  for replay.
- `dynamodb` — set `DYNAMODB_ENDPOINT`; pair with IAM role for the host.
- `foundationdb` — set `FDB_CLUSTER_FILE`; FDB cluster must be
  reachable.

Each asset binary should run against its own logical namespace (Redis
keyspace prefix, DynamoDB table, or FDB tenant) so the four don't
conflict.

### Postgres (referee only)

Used by: `referee` when built with `--features postgres`. The asset
binaries do **not** use Postgres.

Run on a cluster physically distinct from any asset storage so a
compromise of asset infrastructure does not leak the audit log.
Migrations under `referee/migrations/`. Audit log + swap state +
unlogged secret-nonce table — see
[`referee/docs/deployment.md`](../referee/docs/deployment.md#postgres-schema).

Backup: nightly `pg_basebackup` + WAL streaming for point-in-time
recovery. Audit log is the most critical table — verify backups
restore cleanly weekly.

### KMS

Used by: `referee`. Stores Ed25519 identity, MuSig2 secp256k1 share,
and push HMAC key. AWS KMS, GCP Cloud KMS, or HashiCorp Vault all work.
Boot script fetches into `tmpfs`, the binary reads, then the script
unlinks. The binary never holds raw keys outside its own memory.

### Push provider

Used by: `referee`. Operational concern of the deployer — see
[`referee/docs/push-notification.md`](../referee/docs/push-notification.md)
for the webhook contract. Web Push, FCM, APNs, or a custom relay all
satisfy the contract. The push provider is responsible for delivering
the JSON envelope to the recipient wallet via whatever transport its
SDK uses.

### ARK ASP

Used by: extro-node wallets that participate in referee-mediated
Webcash↔ARK swaps. The referee itself does NOT integrate with an ARK
ASP — the integration is on the wallet side (Alice's ARK vtxo lives in
her wallet's chosen ASP). The referee only sees the abstract `vtxo`
hash and the canonical `TX_settle` / `TX_refund` hashes.

## Reverse proxy

Single fronting proxy (nginx/Caddy/Cloudflare) with TLS. Each backend
service routes by hostname:

```nginx
server { server_name webcash.example;        location / { proxy_pass http://127.0.0.1:8080; } }
server { server_name rgb.example;            location / { proxy_pass http://127.0.0.1:8081; } }
server { server_name rgb-collectible.example; location / { proxy_pass http://127.0.0.1:8082; } }
server { server_name voucher.example;        location / { proxy_pass http://127.0.0.1:8083; } }
server { server_name referee.example;        location / { proxy_pass http://127.0.0.1:8090; } }
```

Rate-limit aggressively at the proxy: `referee` /v1/swap/initiate is the
expensive endpoint (background spawns, ZKP verification); cap to
something like 10 req/s per IP and 100 concurrent in-flight swaps per
IP to bound load.

## Authentication boundaries

| Endpoint | Auth |
|---|---|
| `webcash /api/v1/*` | Public (matches webcash.org) |
| `rgb /api/v1/*` | Public read; mint requires issuer signature in body |
| `voucher /api/v1/*` | Public read; mint requires issuer signature |
| `referee /v1/pubkey` | Public |
| `referee /v1/swap/initiate` | Public (rate-limited at proxy) |
| `referee /v1/swap/{id}/audit` | Public (read-only) |
| `referee /v1/swap/{id}/poll` | Public |
| `referee /v1/swap/{id}/ack` | HMAC over body via push provider |

No bearer tokens, no per-user accounts. The asset binaries are
content-addressed by issuer pubkey; the referee is a public utility.

## Observability

All binaries emit `tracing-subscriber` JSON logs to stdout. Pipe to the
deployer's log aggregator. Recommended Prometheus counters per binary:

```
webycash_replace_total{asset="...",outcome="..."}
webycash_mint_total{asset="..."}
webycash_zkp_rejected_total           # referee
webycash_pre_check_spent_total        # referee
webycash_insert_push_retries_total    # referee
```

A sidecar exporter is preferred (`node_exporter` + a small
`tracing-prometheus` adapter) so the binaries don't depend on a
specific metrics shape.

## Health checks

| Service | Probe |
|---|---|
| webcash / rgb / rgb-collectible / voucher | `POST /api/v1/health_check` with a small token-list body (matches webcash.org's contract — body required even for liveness; an empty list returns 200 cheaply) |
| referee | `GET /v1/pubkey` (200 = signing keys loaded) |

`/api/v1/target` is a `GET` and works as a load-balancer health probe
without a body, but reaches less of the request stack than
`/api/v1/health_check`. Pick `target` for fast liveness checks and
`health_check` for deeper probes.

## Deploy ordering

1. Provision data layers: chosen asset-storage backend (Redis /
   DynamoDB / FoundationDB) for the four asset binaries; referee
   Postgres cluster; KMS keys.
2. Deploy `webycash-server-webcash`.
3. Deploy `webycash-server-rgb` and `webycash-server-rgb-collectible`.
4. Deploy `webycash-server-voucher`.
5. Provision push provider; configure the webhook URL.
6. Deploy `referee` last (it depends on `rgb` being reachable for the
   swap-tracking record + `webcash.org` for spent-status checks).
7. Configure proxy + DNS to expose each by hostname.

Take down in reverse order.

## Smoke tests after deploy

```sh
# Each asset reachable (target is a no-body GET; health_check is a POST)
curl -fsS https://webcash.example/api/v1/target
curl -fsS https://rgb.example/api/v1/target
curl -fsS https://rgb-collectible.example/api/v1/target
curl -fsS https://voucher.example/api/v1/target
curl -fsS -X POST -H 'content-type: application/json' \
    -d '{"webcash":[]}' https://webcash.example/api/v1/health_check
curl -fsS https://referee.example/v1/pubkey | jq .

# Synthetic webcash replace round-trip
# (issued + replaced webcash on the test mainnet)
./scripts/smoke/webcash_replace.sh

# Synthetic referee swap with test wallets
# (ZKP-verified, MuSig2-cosigned, against the testnet ASP)
./scripts/smoke/referee_e2e.sh
```

A passing smoke run is the deploy gate.
