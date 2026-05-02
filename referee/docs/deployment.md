# Referee — deployment

## Build

```sh
cd webycash-server
cargo build --release -p referee
```

Optional features:

- `--features zkp-arkworks` — plug in the real Groth16 verifier (BN254).
- `--features musig2-real` — plug in the real MuSig2 signer (musig2 crate
  + secp256k1).
- `--features postgres` — plug in the Postgres-backed `SwapStore` and
  audit log persistence.

For production, build with all three: `--features zkp-arkworks,musig2-real,postgres`.

## Required environment variables

| Var | Purpose |
|---|---|
| `REFEREE_BIND` | `host:port` to bind the HTTP API on (e.g. `0.0.0.0:8090`). |
| `REFEREE_IDENTITY_KEY_PATH` | File with hex-encoded 32-byte Ed25519 secret. 0600 permissions. |
| `REFEREE_RGB_SERVER_URL` | Base URL of `webycash-server-rgb` (the swap-tracking record sink). |
| `REFEREE_WEBCASH_SERVER_URL` | Base URL of webcash.org (`https://webcash.org`). |
| `REFEREE_PUSH_WEBHOOK_URL` | Webhook URL of the deployer's push provider. |
| `REFEREE_PUSH_WEBHOOK_HMAC_KEY_PATH` | File with hex 32-byte HMAC key shared with the push provider. 0600. |

## Optional environment variables

| Var | Default | Purpose |
|---|---|---|
| `REFEREE_POSTGRES_URL` | unset (in-memory store) | Postgres connection URL when feature `postgres` is enabled |
| `REFEREE_SWAP_MAX_AGE_SECS` | `86400` | Maximum lifetime of a swap before forced abort |
| `REFEREE_INSERT_PUSH_RETRY` | `3` | Maximum insert-push retries before abort |
| `REFEREE_RETRY_BACKOFF_MS` | `250` | Exponential backoff base between retries |
| `RUST_LOG` | `info` | tracing-subscriber filter |

## Boot sequence

1. Load + validate config from env (`Config::from_env`).
2. Read identity key file → construct `Identity`.
3. Read HMAC key file → decode hex → construct `HttpPush`.
4. Construct (production) `ArkworksVerifier` with bundled BN254
   verifying keys for both circuits.
5. Construct (production) real MuSig2 signer with secp256k1 secret
   loaded from KMS.
6. Connect to Postgres (if `postgres_url` set), apply migrations.
7. Wire up `Orchestrator`.
8. Build `axum::Router`, bind, serve.

`main.rs` performs steps 1–8 in sequence; failure at any step prints a
diagnostic and exits non-zero.

## Postgres schema

Migrations under `referee/migrations/`. At a minimum:

```sql
-- Latest swap state per id (UPSERT target).
CREATE TABLE referee_swaps (
    id            TEXT PRIMARY KEY,
    phase         TEXT NOT NULL,
    state_json    JSONB NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL,
    terminal      BOOLEAN NOT NULL DEFAULT FALSE
);

-- Audit log: append-only.
CREATE TABLE referee_audit_log (
    seq           BIGSERIAL PRIMARY KEY,
    swap_id       TEXT NOT NULL,
    phase         TEXT NOT NULL,
    ts_unix       BIGINT NOT NULL,
    prior_tip     TEXT NOT NULL,
    phase_payload JSONB NOT NULL,
    signature     TEXT NOT NULL,
    INDEX (swap_id, seq)
);

-- Mu Sig2 secret nonces — local-only, NEVER replicated.
-- A separate table on a separate tablespace; backups exclude it.
CREATE UNLOGGED TABLE referee_secret_nonces (
    swap_id       TEXT NOT NULL,
    session       TEXT NOT NULL CHECK (session IN ('settle', 'refund')),
    secnonce_blob BYTEA NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (swap_id, session)
);
```

Note `UNLOGGED` for secret nonces: in case of crash, in-flight nonces
are *intentionally* lost (they would otherwise be a stale-nonce
liability). Crashed swaps must be restarted, not resumed mid-MuSig2.

## Key management

| Key | How to store |
|---|---|
| Ed25519 identity | KMS-backed; pulled to a tmpfs file at boot, removed after `Identity::load_from_file` succeeds |
| MuSig2 secret share | KMS-backed; same flow |
| Push HMAC key | Sealed file from secret manager; tmpfs at boot |
| Postgres credentials | Standard secret manager flow |

For dev / testnet, all four can be plain files in `/etc/referee/`. For
production, all four MUST come from a KMS or comparable.

## Reverse proxy

Recommended deployment: behind nginx / Caddy with TLS termination. The
referee binary speaks plain HTTP; TLS, rate-limiting, and access logs
are operational responsibilities of the proxy.

Example nginx:

```nginx
upstream referee {
    server 127.0.0.1:8090;
    keepalive 8;
}

server {
    listen 443 ssl http2;
    server_name referee.example;

    location /v1/ {
        proxy_pass http://referee/v1/;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_read_timeout 60s;
    }
}
```

## Health check

The referee does not yet expose a dedicated health endpoint. `/v1/pubkey`
serves as a liveness probe (200 = process alive, signing keys loaded).

## Backup + recovery

- **Audit log**: critical, must be backed up. Postgres logical
  replication or pg_basebackup. Loss = compliance failure.
- **Swap state (terminal rows)**: nice-to-have. In-flight swaps that
  crash are aborted (see `referee_secret_nonces` UNLOGGED note above).
- **Identity key**: backed up to KMS' own redundancy. Re-deriving from
  a backup for a new node is the standard rotation flow.

## Upgrades

1. Drain: stop accepting new `/v1/swap/initiate` requests (firewall the
   port; existing swaps complete or abort within `swap_max_age_secs`).
2. Wait until all swaps are in terminal phase.
3. Upgrade the binary.
4. Resume.

Never restart the referee with in-flight swaps mid-MuSig2 — the
unlogged secret-nonce table loses its rows and the orchestrator can no
longer complete partial-sig generation. The wallet implementors should
fall back to their HTLC timeout refund paths; document this clearly to
operators.

## Observability

The referee emits structured JSON logs via `tracing-subscriber` (set
`RUST_LOG=info,referee=debug` for debug-level swap traces). Each swap's
log lines carry the `swap_id` field for correlation.

For metrics, expose a Prometheus exporter via the deployer's choice of
sidecar (we don't bundle one). Recommended counters:

- `referee_swaps_initiated_total`
- `referee_swaps_settled_total`
- `referee_swaps_refunded_total`
- `referee_zkp_rejected_total`
- `referee_pre_check_already_spent_total`
- `referee_insert_push_retries_total`

## Testing in production

Smoke tests after deployment:

1. `curl https://referee.example/v1/pubkey` — returns 200 with the
   expected pinned pubkey.
2. Run a synthetic swap end-to-end with a test wallet pair against a
   testnet vtxo + testnet webcash — should complete in <2 minutes.
3. Verify the audit log entry chain via the read-only endpoint.
4. Disconnect push provider; confirm a swap aborts and refunds within
   `REFEREE_INSERT_PUSH_RETRY × REFEREE_RETRY_BACKOFF_MS` timeout.
