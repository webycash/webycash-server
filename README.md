# webycash-server

Open-source webcash protocol server implementation in Rust.

## Architecture

- **HTTP**: hyper 1.x with HTTP/1.1 + HTTP/2 support
- **Actors**: ractor (Erlang-inspired gen_server, supervisor trees)
- **Databases**: DynamoDB, Redis, FoundationDB, Redis+FDB (generic adapter)
- **Parsing**: nom parser combinators for webcash token validation
- **Effects**: Free monad pattern for composable, testable ledger operations

## API Endpoints

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/target` | Mining difficulty and parameters |
| POST | `/api/v1/mining_report` | Submit proof-of-work solution |
| POST | `/api/v1/replace` | Atomic token exchange (transfer) |
| POST | `/api/v1/health_check` | Check token spent/unspent status |
| POST | `/api/v1/burn` | Permanently destroy tokens |
| GET | `/api/v1/stats` | Economy statistics |
| GET | `/terms` | Terms of service |

## Quick Start

```bash
# Start Redis (or FoundationDB, or DynamoDB Local)
docker compose up redis -d

# Run the server
cargo run -- --config config/testnet.toml
```

## Docker Compose (Local Development)

```bash
# Redis only
docker compose up redis -d

# FoundationDB only
docker compose up foundationdb fdb-init -d

# Redis + FoundationDB
docker compose up redis foundationdb fdb-init -d

# DynamoDB Local
docker compose up dynamodb-local -d
```

## Configuration

See `config/testnet.toml` and `config/production.toml`.

**Testnet mode**: Constant low difficulty (16 bits). CPU mines in seconds.

**Production mode**: Dynamic difficulty adjustment per epoch.

## Platforms

Linux and FreeBSD.

## License

MIT
