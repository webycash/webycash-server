# webycash-server

Open-source webcash protocol server implementation in Rust.

> **Branch `refactor/asset-traits`**: this workspace is being generalised
> from a single webcash binary into an **asset-gated server family**
> producing four binaries — `server-webcash`, `server-rgb`,
> `server-rgb-collectible`, `server-voucher` — each specialising one
> asset flavor at build time. See
> [ROADMAP `v0.4.0`](ROADMAP.md#v040--asset-gated-server-family-refactorasset-traits-branch)
> and [CHANGELOG `[Unreleased]`](CHANGELOG.md) for the full picture.

## Architecture

- **HTTP**: hyper 1.x with HTTP/1.1 + HTTP/2 support
- **Actors**: ractor (Erlang-inspired gen_server, supervisor trees)
- **Databases**: DynamoDB, Redis, FoundationDB, Redis+FDB (generic adapter)
- **Parsing**: nom parser combinators for webcash token validation
- **Effects**: Free monad pattern for composable, testable ledger operations
- **Asset traits**: `Asset`, `SplittableAsset`, `TransferableAsset`,
  `IssuedAsset`, `MintableAsset` hierarchy in `webycash-asset-core` —
  each binary specialises a generic `Server<A: Asset, S: LedgerStore<A>>`
- **Issuer auth**: Ed25519 raw + OpenPGP V4 armored cert support
  (rpgp 0.19) for RGB and Voucher `/api/v1/issue`

## Binaries (refactor/asset-traits)

| Binary | Asset | Splittable | Namespace | Mining-only? |
|--------|-------|------------|-----------|--------------|
| `webycash-server-webcash` | Webcash | yes | none (frozen) | yes |
| `webycash-server-rgb` | RGB20 fungible | yes | `(contract_id, issuer_fp)` | no |
| `webycash-server-rgb-collectible` | RGB21 NFT | no (1:1) | `(contract_id, issuer_fp)` | no |
| `webycash-server-voucher` | Voucher | yes | `(contract_id, issuer_fp)` | no |

## API Endpoints

Every binary exposes the same endpoint set; only the wire format and
namespace enforcement differ.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/target` | Mining difficulty and parameters |
| POST | `/api/v1/mining_report` | Submit proof-of-work solution |
| POST | `/api/v1/replace` | Atomic token exchange — server is a single-use-seal registry |
| POST | `/api/v1/health_check` | Check token spent/unspent status |
| POST | `/api/v1/burn` | Permanently destroy tokens |
| POST | `/api/v1/issue` | Operator-signed mint (RGB / Voucher only; Ed25519 or OpenPGP V4) |
| GET | `/api/v1/stats` | Economy statistics |
| GET | `/terms` | Terms of service |

## Token wire formats

| Asset | Splittable | Secret form |
|-------|------------|-------------|
| Webcash | yes | `e{amount}:secret:{hex64}` |
| RGB20 (fungible) | yes | `e{amount}:secret:{hex64}:{contract_id}:{issuer_pgp_fp}` |
| RGB21 (collectible) | no | `secret:{hex64}:{contract_id}:{issuer_pgp_fp}` |
| Voucher | yes (always) | `e{amount}:secret:{hex64}:{contract_id}:{issuer_pgp_fp}` |

Public forms swap `:secret:` → `:public:` and the hex secret → its
`sha256(secret_hex_bytes)`. Webcash format is **frozen** by the live
conformance suite against `https://webcash.org`.

## Quick Start

```bash
# Start Redis (or FoundationDB, or DynamoDB Local)
docker compose up redis -d

# Run the server (legacy single-binary)
cargo run -- --config config/testnet.toml

# Or, on refactor/asset-traits, run a specific flavor:
cargo run --release -p webycash-server-webcash
cargo run --release -p webycash-server-rgb
cargo run --release -p webycash-server-rgb-collectible
cargo run --release -p webycash-server-voucher
```

## Docker Compose (Local Development)

```bash
# Legacy single-binary setup
docker compose up redis -d
docker compose up foundationdb fdb-init -d
docker compose up dynamodb-local -d

# refactor/asset-traits: bring up all four flavors at once
docker compose -f docker-compose.local.yml up -d
# → server-webcash on :8181, server-rgb on :8182,
#   server-voucher on :8183, server-rgb-collectible on :8184
```

## Configuration

See `config/testnet.toml` and `config/production.toml`.

**Testnet mode**: Constant low difficulty (16 bits). CPU mines in seconds.

**Production mode**: Dynamic difficulty adjustment per epoch.

## Platforms

Linux and FreeBSD.

## License

MIT
