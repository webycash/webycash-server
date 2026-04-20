# Changelog

All notable changes to `webycash-server` are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [0.1.0] - 2026-04-21

### Fixed
- **Mining validation**: Accept multiple webcash outputs (match webcash.org behavior).
- **Float timestamps**: Accept floating-point timestamps in mining preimage (C++ webminer compat).
- **Token deduplication**: Deduplicate token inserts when C++ format duplicates subsidy hash.
- **Base64 preimages**: Try base64 decode first, then raw JSON fallback (GPU WorkUnit format).
- **CI**: Add `contents:write` permission for release artifact upload.
- **FoundationDB**: Add `boot()` call, fix docker-compose networking.

### Added
- Production Dockerfile (Alpine) with multi-stage build.
- Terraform Kubernetes module (`terraform/webcash-server-k8s/`).
- CI pipeline: test on push, cross-compile release binaries (Linux x86/arm64, FreeBSD x86).

### Initial (2026-04-14)

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
- Platform enforcement (Linux + FreeBSD for production, macOS for development)
- Security: overflow-safe amounts, atomic DB operations, subsidy validation, timestamp validation
