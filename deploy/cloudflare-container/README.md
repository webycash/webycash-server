# Cloudflare Workers + Containers deployment

The "Lambda-equivalent" deployment path: the **existing native hyper
binary** runs inside a Cloudflare Container, fronted by a thin Worker
that forwards public traffic to it and serves D1 to the Container through a
container-only outbound bridge (`workers/shared/d1-bridge.ts`). The public
Worker origin has no database route.

```
       Internet                  Cloudflare edge
  client ──────► Worker (TS) ── /* ──► Container.fetch(req)
                  (no DB route)                     ┌──────────────────────┐
                                                    │ webycash-server-X    │
   container-only outbound                          │   (hyper + tokio)    │
   http://d1.webycash.internal/__d1/batch ◄──────── │   WorkerProxyStore   │
     (outboundByHost, Bearer token) ─► D1 (SQLite)  └──────────────────────┘
```

The Container is a normal Linux x86_64 environment with full hyper,
tokio, and any other Rust dependency — same binary that runs on bare
metal, AWS Lambda, or any traditional VM. The single piece of
Cloudflare-native plumbing is the D1 bridge: `@cloudflare/containers`
`outboundByHost` intercepts the Container's requests to the virtual origin
`http://d1.webycash.internal` and serves `/__d1/batch` inside the Workers
runtime (Bearer `WORKER_PROXY_AUTH_TOKEN`, a Worker secret of >= 32 chars;
both sides fail closed without it). This lets the Container reach D1 (whose binding is Worker-only) without
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
# Put the printed database_id values into ../../environments.json
# (the one manifest; wrangler.toml is a template and holds no ids)

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
calls `http://d1.webycash.internal/__d1/batch` for every storage
operation; the Worker's outbound handler serves it from the local D1 binding.
Put `WORKER_PROXY_AUTH_TOKEN=<random 32+ chars>` in `.dev.vars`.

## Deployment

```sh
cd deploy/cloudflare-container/workers/{flavor}

# once per environment: the Container <-> Worker D1 bridge token
openssl rand -hex 32 | wrangler secret put WORKER_PROXY_AUTH_TOKEN

export CLOUDFLARE_ACCOUNT_ID=...   # never committed

# testnet
npm run deploy

# production
npm run deploy:prod
```

`npm run deploy[:prod]` renders `wrangler.generated.toml` from
`wrangler.toml` + `../../environments.json` and refuses (non-zero exit)
while a D1 id is missing, a `REPLACE_ME` placeholder remains, an
`account_id` is in the repo, or `env.production` lacks its
`durable_objects` binding. Do not run bare `wrangler deploy` on the
template.

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

`POST http://d1.webycash.internal/__d1/batch` (container-only) with
`Authorization: Bearer $WORKER_PROXY_AUTH_TOKEN` and body:

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
