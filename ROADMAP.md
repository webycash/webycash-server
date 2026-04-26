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
- [x] 132 lib tests + 9 doctests across the workspace
- [x] 12 conformance integration tests against live Docker compose
  (lifecycle for each flavor × Redis + DynamoDB, signed `/issue`,
  OpenPGP V4 armored cert `/issue`, live webcash.org)
- [x] 59 property tests across the security/correctness boundary:
  14 wire-format, 8 storage-key, 7 HashRecord codec, 6 Amount,
  6 auth, 8 mining, 4 compute, 6 shared-parser
- [x] 6 production fixture invariants pinning the webcash.org
  Tornado-style quirks (text/html for JSON, legalese.terms required)
- [x] Workspace clippy clean with `--tests`

### Operational
- [x] Dockerfile.flavor HEALTHCHECK against /api/v1/target
- [x] Redis healthcheck (redis-cli ping) + service_healthy depends_on
  in docker-compose.local.yml — no connect-fail-and-retry race on
  cold start

### Wallet (webylib companion repo)
- [x] `Wallet<A: Asset>` core + `wallet-{webcash,rgb,voucher}` flavors
- [x] `webyca` multi-asset CLI with **11 verbs**: flavor-tagged
  (webcash/rgb/voucher × pay-or-transfer/insert), flavor-agnostic
  (target, stats, check, burn, mining-report), local-only
  (derive-public, verify)
- [x] 19 CLI parse-time tests + 8 e2e tests against running compose
- [x] Three storage backends: `MemStore`, `JsonStore`, `SqliteStore`
  with cross-backend conformance tests (11 scenarios × 3 backends)
- [x] WASM wallet target with client-side AluVM contract execution
  (validation runs in-browser before `/replace` is submitted)

### Open follow-ups
- [x] Snapshot v2 namespace fields on UnspentOutputSnapshot +
  SpentHashSnapshot (additive; V1 snapshots load unchanged).
  Future: full SnapshotV2 wallet-side migration logic when
  wallet-rgb / wallet-voucher start writing to webylib-storage.
- [x] Parser fuzz suite (`crates/conformance/tests/fuzz_parsers.rs` —
  stable-Rust proptest, no nightly cargo-fuzz toolchain required;
  PROPTEST_CASES env var lets CI dial up to ~1M cases per parser)
- [ ] AluVM script-entry fuzz (separate scope)
- [ ] Fold the legacy webcash-only `webyc` CLI into `webyca`
- [x] Parser microbench landed
  (`crates/conformance/tests/parser_bench.rs`). Catches a 10x
  regression in the parser layer without a running server.
  Sample throughput: ~1.7M parse/s Webcash, ~1.1M parse/s RGB20,
  ~2.2M parse/s RGB21, ~1.5M parse/s Voucher.
- [ ] End-to-end bench parity check (≥12.7k TPS Webcash, ≥5k TPS
  RGB/Voucher) — requires the existing legacy throughput.rs bench
  to be ported to each new flavor binary
- [ ] Vendored RGB20 / RGB21 Contractum schemas + AluVM bytecode
- [x] Webcash::build_records: preimage parsed in-trait
  (asset-webcash now has zero Unimplemented stubs)
