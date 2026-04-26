# Roadmap

## v0.1.0 — Foundation (Current)
- [x] Repository structure and Cargo workspace
- [x] Protocol types (Amount with overflow-safe arithmetic, SecretWebcash, PublicWebcash)
- [x] nom parser combinators for token validation
- [x] SHA256 proof-of-work verification
- [x] Difficulty adjustment (constant testnet, dynamic production)
- [x] LedgerStore trait — generic database adapter
- [x] Redis backend with Lua scripts for atomic operations
- [x] DynamoDB backend with TransactWriteItems + condition expressions
- [x] FoundationDB backend (behind `--features fdb`, requires FDB C client)
- [x] Redis+FDB composite backend (behind `--features fdb`)
- [x] ractor actor hierarchy (Ledger, Miner, Stats)
- [x] Supervisor with one-for-one restart via spawn_linked
- [x] Handle/Service middleware pattern (Logged, Timed, HandlerService)
- [x] Free Monad effect system (LedgerEffect + interpreter, used by replace)
- [x] `#[gen_server]` proc macro — generates Actor + Message + Handle from impl block
- [x] hyper 1.x HTTP server with HTTP/1.1 + HTTP/2
- [x] SSE streaming endpoint (POST /api/v1/mining_report/stream)
- [x] All 7 webcash protocol endpoints
- [x] TOML + env config, testnet/production modes
- [x] Docker Compose (Redis, FDB, DynamoDB Local)
- [x] Security audit: atomic ops, overflow-safe amounts, subsidy validation
- [ ] Integration tests for FoundationDB backend (requires FDB C client on CI)
- [ ] Integration tests for Redis+FDB composite backend
- [ ] `#[supervisor]` proc macro (supervision currently hand-written)

## v0.2.0 — Testnet Deployment
- [x] AWS Lambda integration via backend repository
- [x] DynamoDB tables (WebcashTokens, WebcashMiningState, WebcashAuditLog)
- [x] weby.cash frontend with developer pages
- [ ] EventBridge keep-warm scheduler
- [ ] End-to-end webylib tests against deployed testnet

## v0.3.0 — Production Hardening
- [ ] Rate limiting middleware
- [ ] Prometheus metrics export
- [ ] Comprehensive documentation
- [ ] FreeBSD CI testing

## v0.4.0 — Asset-gated server family (`refactor/asset-traits` branch)
Generalises the webcash-only server into a workspace that produces four
binaries from one core, each specialising a single asset flavor at
build time. See CHANGELOG `[Unreleased]` for full details.

### Asset-core
- [x] `Asset`, `SplittableAsset`, `TransferableAsset`, `IssuedAsset`,
  `MintableAsset` trait hierarchy
- [x] `RecordBuilder` / `CollectibleRecordBuilder` for per-flavor
  mint+replace records
- [x] Token wire format frozen for Webcash; namespaced
  `e{amt}:secret:{hex}:{contract}:{issuer_fp}` for RGB / Voucher
- [x] Strict-EOF parsers across all eight `FromStr` impls (proptest-found
  bug fixed)

### Asset implementations
- [x] `webycash-asset-webcash` (frozen against webcash.org production)
- [x] `webycash-asset-rgb` (RGB20 fungible + RGB21 collectible)
- [x] `webycash-asset-voucher` (always-splittable bearer credits)

### Storage
- [x] Generic `LedgerStore<A>` over four backends (Redis, DynamoDB,
  FoundationDB, Redis+FDB)
- [x] `KeyStrategy` with `WebcashLegacyKeys` (frozen) and
  `NamespacedKeys` (`(asset, contract_id, issuer_fp, public_hash)`)
- [x] Cross-asset / cross-namespace key uniqueness property-tested

### Auth
- [x] `webycash-auth` Ed25519 signature verification + nonce cache
- [x] `add_pgp_armored` parses OpenPGP V4 certs (rpgp 0.19) and
  registers the primary Ed25519 key under its V4 fingerprint
- [x] `WEBYCASH_ISSUER_PGP_CERTS` wired into all three issued-asset
  binaries (server-rgb, server-rgb-collectible, server-voucher)

### Server core
- [x] Generic `Server<A: Asset, S: LedgerStore<A>>` with hyper HTTP/1.1+H2
- [x] Single endpoint family: `/api/v1/{target,health_check,replace,burn,mining_report,issue}`
- [x] Server is single-use-seal registry — no server-side AluVM, no
  `/transfer`, just atomic `(verify input unspent) → (mark spent +
  insert output)` within `(contract_id, issuer_fp)` namespace
- [x] Replace blanket-impl gated on `SplittableAsset` /
  `TransferableAsset`; cross-namespace replace returns 422

### Binaries
- [x] `server-webcash`, `server-rgb`, `server-rgb-collectible`,
  `server-voucher` — one Cargo build target each
- [x] `Dockerfile.flavor` parameterised; `docker-compose.local.yml`
  runs all four locally (each on its own Redis DB plus shared
  DynamoDB Local + optional FoundationDB)

### Conformance + tests
- [x] 12 conformance integration tests against live Docker compose
  (lifecycle for each flavor × Redis + DynamoDB, signed `/issue`,
  OpenPGP V4 armored cert `/issue`, live webcash.org)
- [x] 14 wire-format property tests (proptest, 256–2048 cases each)
- [x] 8 storage-key partitioning property tests
- [x] 6 Amount arithmetic property tests
- [x] Workspace clippy clean with `--tests`

### Wallet (webylib companion repo)
- [x] `Wallet<A: Asset>` core + `wallet-{webcash,rgb,voucher}` flavors
- [x] `webyca` multi-asset CLI: `webyca {webcash|rgb|voucher} {pay|transfer|insert}`
- [x] Three storage backends: `MemStore`, `JsonStore`, `SqliteStore`
  with cross-backend conformance tests
- [x] WASM wallet target with client-side AluVM contract execution
  (validation runs in-browser before `/replace` is submitted)

### Open follow-ups
- [ ] Snapshot/restore extension with `asset_type` + namespace fields
- [ ] Fuzz harnesses (`cargo-fuzz`) for parsers and AluVM script entry
- [ ] Fold the legacy webcash-only `webyc` CLI into `webyca`
- [ ] Bench parity check (≥12.7k TPS Webcash, ≥5k TPS RGB/Voucher)
- [ ] Vendored RGB20 / RGB21 Contractum schemas + AluVM bytecode
