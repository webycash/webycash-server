# Changelog

All notable changes to `webycash-server` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

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
