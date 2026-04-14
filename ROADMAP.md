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
