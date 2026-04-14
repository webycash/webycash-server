# Initial Vision

This document preserves the original design prompt that initiated the webycash-server project and the broader webycash ecosystem rebuild.

## Original Requirements

Build a new repository named `webycash-server` in the webycash folder with git origin at `git@github.com:webycash/webycash-server.git`.

Inspired by the maaku/webminer repository which implements a webcash server. The server runs under `weby.cash/api/webcash/testnet` as a proxy to Lambda functions under `/api/v1/webcash/testnet`. The testnet server implements the same API as the original webcash server but with constant low mining difficulty so developers can mine with CPUs in seconds.

### Architecture Decisions

- **HTTP Server**: hyper (not actix-web) as the REST frontend
- **Actor Model**: ractor for Erlang-inspired gen_server/supervisor semantics
- **Databases**: 3 backends — DynamoDB, Redis, FoundationDB — with generic adapter trait
- **Redis+FDB**: Redis as cache layer in front of FoundationDB for production speed
- **HTTP/2**: Full support including streaming for mining reports
- **Platforms**: Linux + FreeBSD only
- **License**: MIT

### Design Patterns

1. **Free Monad Pattern** — composable effect descriptions for ledger operations
2. **Parser Combinators (nom)** — declarative parsing of webcash tokens and API requests
3. **Handle/Service Pattern** — traits + blanket impls for service composition
4. **OTP Behaviours via proc macros** — `#[gen_server]` derive for actor message dispatch
5. **Supervision Trees** — `#[supervisor(one_for_one)]` for actor hierarchy with restart
6. **Kolmogorov minimal complexity** — shortest possible clean code

### Ecosystem Integration

- **webylib**: Add NetworkMode (Production/Testnet/Custom), light CPU miner, testnet tests
- **backend**: Archive Lightning bridge, integrate webycash-server as testnet on DynamoDB/Lambda
- **frontend**: Remove Bitcoin buying, add developer navigation (SDK, Reference, Server, Testnet), restrict geo-blocking to future p2pex only
- **webycash-sdk**: No changes needed (wraps webylib via FFI)

### URL Scheme

- webylib endpoint constants unchanged: `/api/v1/health_check`, `/api/v1/replace`, etc.
- Testnet base_url: `https://weby.cash/api/webcash/testnet`
- Full URL: `https://weby.cash/api/webcash/testnet/api/v1/health_check`
- Frontend proxy strips `/api/webcash/testnet/` prefix before forwarding to Lambda
- Lambda receives standard `/api/v1/health_check` paths
- Standalone server (docker compose): `http://localhost:8080/api/v1/health_check`

### Testing Strategy

- Docker Compose with Redis, FoundationDB, DynamoDB Local
- Integration tests for all 4 database configurations
- webylib testnet tests: mine → insert → pay → check → merge → recover
- End-to-end: frontend → proxy → Lambda → webycash-server → DynamoDB
