# crates/

Workspace layout for the asset-gated server family.

```
asset-core              Trait hierarchy: Asset, SplittableAsset,
                        TransferableAsset, IssuedAsset, MintableAsset,
                        RecordBuilder, CollectibleRecordBuilder.
                        Plus Amount (8-decimal i64 wats) +
                        PgpFingerprint + ContractId types with shape
                        validation.

asset-webcash           Webcash asset: SecretWebcash / PublicWebcash /
                        WebcashRecord, MintableAsset PoW + record
                        building. Wire-protocol-frozen against
                        webcash.org production.

asset-rgb               RGB asset: SecretFungible / PublicFungible
                        (RGB20 splittable) + SecretCollectible /
                        PublicCollectible (RGB21 non-splittable).
                        TransferableAsset namespace check on
                        collectibles. Tracks rgb-core / aluvm-sdk /
                        strict-types ecosystem.

asset-voucher           Voucher asset: SecretVoucher / PublicVoucher,
                        always splittable, issuer-namespaced bearer
                        credits.

proto                   Shared nom parser building blocks
                        (amount_parser, hex64). Reused by every asset
                        crate. Property-tested against arbitrary input.

storage                 Generic LedgerStore<A> trait + four backends:
                        Redis (Lua atomic ops), DynamoDB (TransactWrite),
                        FoundationDB, Redis+FDB composite. KeyStrategy
                        with WebcashLegacyKeys (frozen testnet schema)
                        and NamespacedKeys ((asset, contract_id,
                        issuer_fp, public_hash) partitioning).

auth                    IssuerRegistry: Ed25519 raw + OpenPGP V4 cert
                        parsing (rpgp 0.19) for /api/v1/issue. Plus
                        NonceCache for replay protection.

mining                  MiningMode (Disabled / Fixed / Dynamic) +
                        difficulty adjustment + verify_pow.

compute                 ComputeBackend trait: sha256_batch,
                        verify_pow_batch, derive_public_hash_batch.
                        CPU reference impl; CUDA / wgpu backends
                        re-introduced in M1.

aluvm-runtime           AluVM 0.12 wrapper used by asset-rgb
                        client-side validation (browser via WASM,
                        native via webylib-aluvm).

server-core             Generic Server<A: Asset, S: LedgerStore<A>>
                        + hyper HTTP/1.1+H2 + handler dispatch.
                        Three serve() entry points: serve (splittable),
                        serve_issued (signed /issue), serve_collectible
                        (RGB21 1:1 transfer).

server-webcash          Binary specialising Server<Webcash, _>.
                        Wire-protocol-frozen.
server-rgb              Binary specialising Server<RgbFungible, _>.
                        OpenPGP V4 issuer auth on /issue.
server-rgb-collectible  Binary specialising Server<RgbCollectible, _>.
                        Non-splittable; /replace enforces 1:1.
server-voucher          Binary specialising Server<Voucher, _>.
                        Always-splittable bearer credits.

conformance             Wire-format conformance suite + property
                        tests (proptest, 256–2048 cases each across
                        59 properties) + production fixture invariants
                        + 12 integration tests against live Docker
                        compose + 10 fuzz tests + parser microbench.

macros                  Procedural macros: #[gen_server] (Actor +
                        Message + Handle from impl block),
                        #[supervisor] (one-for-one restart tree),
                        handler! / validate! (API parsing helpers).

server                  Legacy single-binary server. Asset-gated
                        family is the v0.4.0 path; this crate stays
                        for the existing integration tests + benches
                        until cutover.
```

## Building and testing

```bash
# Workspace lib + bin tests (no compose required)
cargo test --workspace --lib

# Workspace doctests
cargo test --workspace --doc

# Conformance suite (requires Docker Compose)
docker compose -f docker-compose.local.yml up -d --build
cargo test -p webycash-conformance

# Parser microbench (release-mode wall clock)
cargo test --release --test parser_bench -- --ignored --nocapture

# Parser fuzz (default 4096 cases each)
cargo test --release --test fuzz_parsers
# Bumped:
PROPTEST_CASES=1000000 cargo test --release --test fuzz_parsers
```

See [ROADMAP.md](../ROADMAP.md#v040--asset-gated-server-family-refactorasset-traits-branch)
and [CHANGELOG.md](../CHANGELOG.md) for the full status of the
`refactor/asset-traits` branch.
