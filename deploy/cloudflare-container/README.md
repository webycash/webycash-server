# Cloudflare Workers + Containers deployment

The "Lambda-equivalent" deployment path: the **existing native hyper
binary** runs inside a Cloudflare Container, fronted by a thin Worker
that handles edge routing and exposes a `/__d1/batch` endpoint the
Container calls back to for D1 storage operations.

```
       Internet                  Cloudflare edge
  client ──────► Worker (TS)                       Container (Linux Docker)
                  │                                  ┌──────────────────────┐
                  ├── /__d1/batch ◄──── HTTP ──────  │ webycash-server-X    │
                  │      │                           │   (hyper + tokio)    │
                  │      └─► D1 (SQLite)             │   WorkerProxyStore   │
                  │                                  └──────────────────────┘
                  └── /*  ─────► Container.fetch(req)
```

The Container is a normal Linux x86_64 environment with full hyper,
tokio, and any other Rust dependency — same binary that runs on bare
metal, AWS Lambda, or any traditional VM. The single piece of
Cloudflare-native plumbing is the Worker's `/__d1/batch` endpoint, which
lets the Container reach D1 (whose binding is Worker-only) without
giving up D1 as the backing store.

## Layout

```
deploy/cloudflare-container/
├── workers/
│   ├── webcash/{wrangler.toml, package.json, tsconfig.json, src/index.ts}
│   ├── rgb/...
│   ├── rgb-collectible/...
│   └── voucher/...
├── migrations/
│   └── 0001_initial.sql       ← shared D1 schema for all 4 flavors
└── README.md                  ← this file

Dockerfile.cf-webcash               ← repo-root Dockerfiles so the build
Dockerfile.cf-rgb                     context picks up the full source tree;
Dockerfile.cf-rgb-collectible         BuildKit cache mounts share the cargo
Dockerfile.cf-voucher                 target dir across flavors
```

The trigger-based atomicity primitives in the migration (`BEFORE UPDATE
OF spent` + the `INSTEAD OF INSERT` trigger on `assert_token_exists`)
turn would-be-silent zero-row conditions inside `db.batch(stmts)` into
real `ABORT`s that roll the whole batch back.

## Prerequisites

- Cloudflare account + `wrangler` ≥ 4.x — `npm i -g wrangler`
- Cloudflare Containers enabled on your account (paid plan)
- Docker (for `wrangler dev --local` and for production image builds)
- Node.js ≥ 18 (for the TS Worker)

## One-time setup per flavor

Replace `{flavor}` with one of `webcash`, `rgb`, `rgb-collectible`,
`voucher`.

```sh
cd deploy/cloudflare-container/workers/{flavor}

# install TS deps
npm install

# Cloudflare auth + D1 provisioning
wrangler login

wrangler d1 create webycash-cf-container-{flavor}-testnet
wrangler d1 create webycash-cf-container-{flavor}-production
# Paste the printed database_id values into wrangler.toml

wrangler d1 migrations apply webycash-cf-container-{flavor}-testnet --remote
wrangler d1 migrations apply webycash-cf-container-{flavor}-production --remote
```

## Local development

```sh
cd deploy/cloudflare-container/workers/webcash
wrangler dev --local
```

First run cross-compiles the native Rust binary inside Alpine (5–10
minutes); subsequent runs hit the Docker layer cache and come up in
~20 seconds.

The Worker listens on `http://127.0.0.1:8787`. Requests to `/api/v1/*`
are forwarded to the Container. The Container's `WorkerProxyStore`
calls `http://host.docker.internal:8787/__d1/batch` for every storage
operation, where Cloudflare's local miniflare serves the D1 binding.

## Deployment

```sh
cd deploy/cloudflare-container/workers/{flavor}

# testnet
wrangler deploy

# production
wrangler deploy --env production
```

`wrangler deploy` builds the Container image from the corresponding
`Dockerfile.cf-{flavor}` at the repo root and pushes both the image
and the Worker script in one shot.

## Issuer keys (RGB / RGB-collectible / Voucher only)

The Webcash flavor is permissionless. The other three need at least one
registered issuer for `/api/v1/issue` to accept anything.

Two env-var formats are accepted, individually or together:

- `WEBYCASH_ISSUERS` — comma-separated `fp:hex_pubkey` entries (raw
  Ed25519 public keys)
- `WEBYCASH_ISSUER_PGP_CERTS` — one or more armored OpenPGP V4 cert
  blocks, concatenated as the value (the value is the cert *content*,
  not a filesystem path; multi-block values are split on the
  `-----BEGIN/END PGP PUBLIC KEY BLOCK-----` markers)

Set them in `wrangler.toml [vars]` (cleartext, fine for testnet) or
via `wrangler secret put` (preferred for production):

```sh
wrangler secret put WEBYCASH_ISSUERS
# paste fp:hex_pubkey,fp:hex_pubkey,...
```

The Worker forwards both env vars to the Container at start time
(see the `Container` subclass in `src/index.ts`).

## Wire protocol between Container and Worker

`POST /__d1/batch` with body:

```json
{
  "statements": [
    {"sql": "INSERT INTO tokens (pk, amount_wats, spent, data) VALUES (?1, ?2, 0, ?3)", "params": ["token:abc", 100000000, "{...}"]}
  ]
}
```

Success response:

```json
{
  "ok": true,
  "results": [{"rows": []}]
}
```

Error response (e.g., trigger ABORT or PK constraint):

```json
{"ok": false, "error": "ABORT: double_spend"}
```

The Worker translates `statements` into `env.WEBYCASH_D1.prepare(sql)
.bind(...params)` and runs the whole array atomically via
`env.WEBYCASH_D1.batch(stmts)`. Any SQLite error inside the batch
rolls everything back and surfaces as `ok: false`.

The `WorkerProxyStore` in the Container maps the `error` string back
to a `ReplaceResult::Failed("double_spend" | "input_missing" |
"output_collision" | <raw>)` diagnostic via
`crate::storage::worker_proxy_backend::diagnostic_from_error`.

## Why D1 via Worker proxy (and not direct)

D1's data-plane binding is exposed only to Workers, not to arbitrary
outbound HTTP. The proxy is the smallest possible piece of glue that
keeps D1 as the database while letting the Container run the unmodified
native Rust hyper binary. Alternatives considered and rejected:

- **Postgres via Hyperdrive** — Container connects directly. Faster
  per call, but you pay a separate Postgres provider; the database
  isn't Cloudflare-native.
- **Durable Objects via the proxy Worker** — strong consistency per
  namespace and microsecond local reads/writes, but more moving parts
  than D1 and harder to migrate state between regions.
