# Changelog

All notable changes to `webycash-server` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.3.0] — 2026-05-02

The `refactor/asset-traits` branch landed. Five binaries from one
workspace; referee swap helper added; HTLC primitive ships on RGB20
and RGB21; full conformance suite green against docker-spawned
servers.

### Added
- **`referee` crate** — Webcash↔Bitcoin ARK swap helper, ZKP-verified
  and MuSig2-cosigned. Typestate `SwapState<P>` with pure transitions;
  signed append-only audit log; `start_swap` background-spawns
  orchestration so `/v1/swap/initiate` returns the swap id immediately.
  Refuses to boot with mock crypto unless `REFEREE_ALLOW_MOCK_CRYPTO=1`.
  Production builds with `--features zkp-arkworks,musig2-real,postgres`.
- **HTLC on `RgbCollectible` (RGB21)** — same machinery as RGB20.
  5-test conformance suite (`server_rgb21_htlc.rs`).
- **`webycash-server/docs/`** — full-stack production deployment guide
  (`deployment.md`), referee-zkp-based-swap protocol spec
  (`referee-zkp-based-swap.md`), and roadmap (`roadmap.md`) with
  honest two-column ship-now vs gated-on framing.
- **CI overhaul** — lint + property + conformance (docker-spawned
  servers) + cross-platform release builds for all five binaries.

### Changed
- Banned phrase **"atomic swap"** removed everywhere; cross-rail flows
  now use specific names: *referee-ZKP-based swap*, *HTLC swap*, *HTLC
  + bearer-race swap*.
- Workspace `default-members = ["crates/server-webcash"]` — the
  default `cargo build` produces the webcash binary only; `rgb`,
  `rgb-collectible`, `voucher`, and `referee` are opt-in via `-p`.
- Push webhook signing replaced hand-rolled HMAC-SHA256 with the
  audited RustCrypto `hmac` crate.

## [Unreleased] — `refactor/asset-traits` branch

Asset-gated server family: one workspace, four binaries
(`server-{webcash,rgb,rgb-collectible,voucher}`). Each binary
specialises a generic `Server<A: Asset, S: LedgerStore<A>>` over the
flavor's wire-format and storage shape.

### Architecture
- **`webycash-asset-core`**: `Asset`, `SplittableAsset`,
  `TransferableAsset`, `IssuedAsset`, `MintableAsset` trait hierarchy +
  `RecordBuilder` / `CollectibleRecordBuilder`.
- **`webycash-proto`**: shared nom parsers (`amount_parser`, `hex64`).
- **`webycash-asset-{webcash,rgb,voucher}`**: per-flavor token types,
  parsers, and `HashRecord` codecs. Webcash wire format is frozen and
  byte-exact against webcash.org production.
- **`webycash-storage`**: generic `LedgerStore<A>` plus four backends
  (Redis, DynamoDB, FoundationDB, Redis+FDB). `KeyStrategy` trait
  preserves the legacy Webcash testnet schema while RGB/Voucher use
  `(asset, contract_id, issuer_fp, public_hash)` partitioning.
- **`webycash-auth`**: Ed25519 issuer signature verification + nonce
  cache; `add_pgp_armored` parses OpenPGP V4 certs (rpgp 0.19) and
  registers the primary Ed25519 key under its V4 fingerprint.
- **`webycash-mining`**: `MiningMode::{Disabled, Fixed, Dynamic}` with
  configurable difficulty / subsidy ratio per flavor.
- **`webycash-aluvm-runtime`**: real AluVM 0.12 wrapper (test-side
  only — RGB validation is client-side per RGB's design).
- **`webycash-server-core`**: `serve` / `serve_issued` /
  `serve_collectible` entry points, hyper HTTP/1.1+H2.
- Single endpoint everywhere: `/api/v1/replace`. Server is a
  single-use-seal registry that sees secrets in transit and persists
  hashes + amounts + namespace. No `/transfer`. No server-side AluVM.

### Issuer authentication
- `WEBYCASH_ISSUERS=fp:hex_pubkey,...` for raw Ed25519 keys.
- `WEBYCASH_ISSUER_PGP_CERTS=/path/to/certs.asc` for ASCII-armored
  OpenPGP V4 certs. Both env vars supported on `server-rgb`,
  `server-rgb-collectible`, `server-voucher`.
- Tampered body → 500. Replayed nonce → 500.
  Cross-namespace `/replace` → 422.

### Production wire format
- All eight `FromStr` impls now require parsers to consume the whole
  input (uncovered by proptest). A namespaced token can no longer be
  silently parsed as plain Webcash.

### Tests
- **132 lib tests + 9 doctests** across the workspace.
- **59 property tests** (proptest, 64–2048 cases each):
  - 14 wire-format parser roundtrips (Webcash, RGB20, RGB21
    collectible, Voucher) plus cross-flavor disjointness pin.
  - 8 storage-key partitioning invariants (cross-asset uniqueness,
    namespace isolation, role-prefix non-collision, legacy-keys frozen).
  - 7 HashRecord codec roundtrips (every persisted record type's
    `to_fields` / `from_fields` agreement, including chrono nanoseconds).
  - 6 Amount arithmetic invariants (overflow, sub, sum-vs-fold,
    Display↔FromStr roundtrip across `i64::MIN/2..=i64::MAX/2`).
  - 6 Auth invariants (arbitrary-body Ed25519 sign/verify, single-byte
    tampering rejection, cross-issuer rejection, nonce replay protection,
    distinct-nonce non-collision, `(fp, nonce)` partitioning).
  - 8 Mining invariants (PoW range, monotonicity, agreement with
    `leading_zero_bits`, difficulty adjustment ±2 clamp, floor at 1,
    equilibrium stability).
  - 4 Compute backend invariants (sha256_batch length + ordered equality
    with sha2, PoW self-consistency, derive-public uniformity).
  - 6 shared `webycash-proto` parser invariants (`amount_parser` and
    `hex64` consume canonical input, stop at separators, reject malformed
    prefixes).
- **6 production fixture invariants** pinning the `webcash.org`
  Tornado quirks (text/html for JSON, legalese.terms required, 4
  numeric fields on get_target, etc.).
- **10 parser fuzz tests** (4096 cases each by default; bump via
  `PROPTEST_CASES=1000000 cargo test --release --test fuzz_parsers`).
  Stable-Rust friendly — no nightly cargo-fuzz toolchain required.
  Catches panic / OOM / silent-consume bugs on arbitrary byte strings
  across every public parser (Webcash, RGB20, RGB21, Voucher).
- **12 conformance integration tests** against live Docker compose
  (lifecycle for each flavor × Redis + DynamoDB, signed `/issue`,
  OpenPGP V4 armored cert `/issue`, live webcash.org).
- Workspace clippy clean with `--tests`.

### Trait surface
- ZERO `Unimplemented` stubs remain in
  `webycash-asset-{webcash,rgb,voucher}`. Every trait method
  (`Asset::*`, `SplittableAsset::*`, `TransferableAsset::validate_transfer`,
  `IssuedAsset::*`, `MintableAsset::{verify_issuance, build_records}`,
  `RecordBuilder::*`, `CollectibleRecordBuilder::*`) has a real
  implementation.

### Rustdoc
- Every public type, trait, free function, and trait method in the
  new asset-trait crates has rustdoc:
  - Server-side: `webycash-asset-core`, `-asset-webcash`, `-asset-rgb`,
    `-asset-voucher`, `-storage`, `-server-core`, `-conformance`,
    `-aluvm-runtime`, `-auth`, `-mining`, `-compute`, `-proto`.
  - Webylib-side: `webylib-wallet-{webcash,rgb,voucher}`,
    `webylib-server-client`, `webylib-cli`, `webylib-storage`.
- `cargo doc --no-deps` is warning-clean across both workspaces.
- **All 12 server-side new crates pass the strict
  `RUSTDOCFLAGS="-W missing-docs" cargo doc --no-deps` lint** —
  every public struct field, enum variant, type alias, trait
  associated type, and trait method carries rustdoc explicitly.
- **All 7 webylib-side new crates also pass strict-docs**
  (`webylib-wallet-{webcash,rgb,voucher}`, `webylib-server-client`,
  `webylib-cli`, `webylib-storage`, `webylib-wasm`). webylib-wasm
  uses `#![allow(missing_docs)]` since its user-visible API is the
  wasm-bindgen-generated JS / .d.ts surface, not the Rust types.

### Deployment
- `Dockerfile.flavor` parameterised by `FLAVOR` build-arg; one image
  per binary (~38 MB each, multi-stage rust:1.92-alpine).
- `docker-compose.local.yml` runs all four flavors locally with
  per-flavor Redis + a shared DynamoDB Local + optional FoundationDB.

### Companion wallet (webylib repo, refactor/asset-traits branch)
- `webylib-wallet-{webcash,rgb,voucher}`: thin asset-flavor verbs
  (`pay` / `transfer` / `insert`) over a shared HTTP `Client`.
- `webyca` multi-asset CLI binary with **11 verbs**:
  - flavor-tagged: `webcash {pay,insert}`, `rgb {transfer,insert}`,
    `voucher {pay,insert}`
  - flavor-agnostic: `target`, `stats`, `check`, `burn`, `mining-report`
  - local-only: `derive-public`, `verify` (no server contact required)
- 19 parse-time + 19 e2e tests across all 3 asset types: 8
  `cli_compose` (basic verb smoke) + 4 `wallet_verbs_compose` (wallet
  API path) + 1 `all_flavors_compose` (full lifecycle for all 4
  flavors) + 6 `full_e2e` (comprehensive coverage matrix: RGB21
  read-only, RGB20+Voucher burn via CLI, cross-namespace replace
  rejection, derive-public / verify against real server state, stats
  counter motion).
- 3-backend `Store` trait conformance suite (`MemStore`, `JsonStore`,
  `SqliteStore` — 11 scenarios × 3 backends).

## [0.2.3] - 2026-04-22

### Architecture
- **Pure functional declarative**: all `for` loops eliminated, `try_fold`/`collect`/`chain` everywhere
- **`#[gen_server]` proc macro**: generates Actor/Message/Handle from handler impl blocks
- **`#[supervisor]` proc macro**: declarative one-for-one restart supervision tree
- **`handler!`/`validate!` macros**: eliminate API body-parsing boilerplate
- **Batch-native `LedgerStore` trait**: every operation is batch-first, single ops are batches of 1
- **Batch coalescing actor**: collects concurrent replace requests, fires single pipelined batch
- **Immutable state transitions**: WebcashServer fully immutable after construction

### Performance (25x improvement)
- **Redis HASH storage**: native HSET/HGET, no JSON encode/decode in Redis
- **Pipelined EVALSHA**: all batch replace operations in single Redis round-trip
- **N+1 to 1 RTT**: replace bypasses Free Monad interpreter, calls atomic_replace directly
- **16-connection pool**: round-robin across connections for max parallelism
- **Zero-copy CORS**: direct header injection, no response rebuild
- **12,731 TPS** replace (3 servers, in-Docker benchmark, up from ~500 TPS)

### DynamoDB optimization
- **Native attributes**: amount_wats (N), spent (BOOL) instead of JSON blob
- **BatchGetItem**: up to 100 items per API call (was N individual GetItem)
- **BatchWriteItem**: up to 25 items per API call (was N individual PutItem)
- **Zero pre-reads**: Update with condition expression (1 RTT, was 2 RTT)
- **ProjectionExpression**: check_tokens only fetches spent field

### Compute backends
- **ComputeBackend trait**: pluggable CPU/CUDA/wgpu
- **CPU**: sha2 + spawn_blocking (default)
- **CUDA**: cudarc, persistent kernel SHA256 (feature = "cuda")
- **wgpu**: Metal/Vulkan/DX12 compute shaders (feature = "wgpu-compute")
- **GPU SHA256**: 1.5M H/s on AMD RX 580 via Metal

### Unified config system
- Three pluggable axes: `[compute]`, `[network]`, `[server.db]`
- `compute.backend = cpu|cuda|wgpu|auto`
- `network.plane = kernel|dpdk`
- Environment variables: `WEBCASH_COMPUTE`, `WEBCASH_NETWORK`, `WEBCASH_DPDK_IFACE`

### Kubernetes DPDK/AF_XDP deployment
- AF_XDP Device Plugin DaemonSet
- NetworkAttachmentDefinition for AF_XDP CNI
- Server StatefulSet with hugepages, CAP_BPF, CAP_NET_RAW
- Redis StatefulSet (io-threads=8, 400GB maxmemory)
- HorizontalPodAutoscaler (3-12 replicas)
- DPDK-ready Docker image (libbpf, libxdp, ethtool)

### Security hardening
- 1MB request body limits (http_body_util::Limited)
- Past timestamp validation (5 min window)
- Configurable CORS (default "*", overridable)
- HTTP/2 settings (max_concurrent_streams, window_size, frame_size)
- 10 penetration tests: concurrent double-spend, replay, overflow, injection

### Testing
- 22 unit tests, 11 integration tests, 10 penetration tests (43 total)
- In-Docker Rust benchmark binary (HTTP/1.1 keep-alive pool, 3 servers)
- GPU compute benchmarks (CPU vs wgpu)

## [0.2.2] - 2026-04-21

### Fixed
- **Token deduplication**: Deduplicate token inserts when C++ format duplicates subsidy hash.
- **Float timestamps**: Accept floating-point timestamps in mining preimage (C++ webminer compat).

### Changed
- Version in Cargo.toml now matches release tags.
- CHANGELOG updated with full version history.

## [0.2.1] - 2026-04-20

### Fixed
- **Mining validation**: Accept multiple webcash outputs (match webcash.org behavior).

## [0.2.0] - 2026-04-19

### Added
- **Base64 preimages**: Try base64 decode first, then raw JSON fallback (GPU WorkUnit format).
- Production Dockerfile (Alpine) with multi-stage build.
- Terraform Kubernetes module (`terraform/webcash-server-k8s/`).

### Fixed
- **CI**: Add `contents:write` permission for release artifact upload.
- **FoundationDB**: Add `boot()` call, fix docker-compose networking.

## [0.1.0] - 2026-04-14

### Added
- Webcash protocol types (Amount with overflow-safe arithmetic, SecretWebcash, PublicWebcash)
- nom parser combinators for token validation
- SHA256 proof-of-work verification with leading zero bit counting
- Difficulty adjustment algorithm (constant for testnet, dynamic for production)
- LedgerStore trait with generic database adapter
- Redis backend with Lua scripts for atomic double-spend prevention
- DynamoDB backend with TransactWriteItems and condition expressions for atomicity
- FoundationDB backend (requires `--features fdb` and FDB C client library)
- Redis+FDB composite backend (write-through cache, requires `--features fdb`)
- ractor actor hierarchy (LedgerActor, MinerActor, StatsActor)
- Supervisor with one-for-one restart strategy via spawn_linked
- Handle/Service middleware pattern (Logged, Timed, HandlerService)
- Free Monad effect system with iterative interpreter (used by replace operation)
- `#[gen_server]` proc macro that generates ractor Actor + Message enum + Handle from handler methods
- hyper 1.x HTTP server with HTTP/1.1 + HTTP/2 auto-negotiation
- SSE streaming endpoint for mining_report (POST /api/v1/mining_report/stream)
- All webcash protocol endpoints (target, mining_report, replace, health_check, burn, stats, terms)
- TOML + environment variable configuration
- Docker Compose for local development (Redis, FoundationDB, DynamoDB Local)
- CI pipeline: test on push, cross-compile release binaries (Linux x86/arm64, FreeBSD x86)
- Platform enforcement (Linux + FreeBSD for production, macOS for development)
- Security: overflow-safe amounts, atomic DB operations, subsidy validation, timestamp validation
