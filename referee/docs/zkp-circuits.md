# Referee ZKP circuits — Groth16 specification

## Why two circuits

The referee verifies *honesty* of two ciphertexts without ever decrypting
them:

- **Bob's payload-honesty circuit**: the encrypted-to-Alice ciphertext
  decrypts under Alice's PGP private key to a 32-byte value `S` whose
  SHA-256 is `H_B` (the public hash already verified unspent on
  webcash.org).
- **Alice's signature-honesty circuit**: the encrypted-to-Bob ciphertext
  decrypts under Bob's PGP private key to a value `sig` that is a valid
  MuSig2 partial-sig by Alice on `TX_settle` under her published
  pubshare and the agreed nonces.

Both circuits use Groth16. The proving system was selected for short
proofs (≈200 bytes), constant verification time, and broad WASM
toolchain support (arkworks + circom).

## Bob's payload-honesty circuit

### Statement

```text
∃ S, sk_A_session, salt :
   pgp_decrypt(EncSec_B_to_A, sk_A_session, salt) = S
   ∧ |S| = 32 bytes
   ∧ sha256(S) = H_B
```

Where:

- `EncSec_B_to_A` is the public PGP ciphertext (committed in the audit log).
- `sk_A_session` is the symmetric session key extracted by hybrid-PGP
  decryption under Alice's PGP private key. Witness only.
- `salt` is whatever the PGP encryption scheme uses (CFB IV, etc.).
- `S` is the cleartext webcash secret. Witness only.
- `H_B` is the public sha256.

### Public inputs

1. `EncSec_B_to_A` (or its hash, if too large for a public-input slot —
   commitment scheme TBD per `arkworks` constraints).
2. `H_B` (32 bytes).
3. Alice's PGP pubkey commitment (so the verifier knows which decryption
   key the witness corresponds to).

### Witness

1. `S` (32 bytes).
2. `sk_A_session` (whatever the hybrid-PGP decryption pulls).
3. PGP padding bytes / IV / salt.

### Constraint estimate

- SHA-256 over 32 bytes: ~25k constraints (arkworks `r1cs-std`).
- PGP hybrid decryption: depends on the symmetric cipher. AES-128-CFB:
  ~10 AES blocks for a small payload, ~13k constraints per block ≈
  130k. ChaCha20: cheaper (~50k total).
- Total: 50k–200k constraints. Within Groth16+BN254 reach on a
  workstation prover (<30 seconds), <500KB WASM verifier bundle.

## Alice's signature-honesty circuit

### Statement

```text
∃ sig, sk_B_session, salt :
   pgp_decrypt(EncSig_A_to_B, sk_B_session, salt) = sig
   ∧ MuSig2_partial_verify(sig,
                           alice_pubshare,
                           tx_settle_hash,
                           pubnonce_A_settle,
                           pubnonce_R_settle,
                           combined_pubkey) = ok
```

Where:

- `EncSig_A_to_B` is the public PGP ciphertext.
- `sig` is Alice's 32-byte MuSig2 partial-signature scalar. Witness only.
- `sk_B_session` is the symmetric session key from hybrid-PGP under
  Bob's private key.
- `MuSig2_partial_verify` checks that `sig` is a valid partial-sig under
  Alice's pubshare given the per-session nonces + combined pubkey.

### Public inputs

1. `EncSig_A_to_B` (or its hash).
2. `alice_musig2_pubshare` (33 bytes compressed secp256k1).
3. `tx_settle_hash` (32 bytes — the message being signed).
4. `pubnonce_A_settle`, `pubnonce_R_settle` (66 bytes each).
5. `combined_pubkey` (33 bytes — derived from the two pubshares,
   recomputed by the verifier from the public inputs above).
6. Bob's PGP pubkey commitment.

### Witness

1. `sig` (32 bytes).
2. `sk_B_session`.
3. PGP padding / salt.

### Constraint estimate

- PGP hybrid decryption: ~50k–130k (same as Bob's circuit).
- MuSig2 partial verify: secp256k1 is **not** SNARK-friendly. Two
  options:
  - **(A) Native secp256k1 in-circuit**: ~1M constraints. Slow proving
    (~5 minutes on a workstation), feasible verifier.
  - **(B) Curve-cycle with BLS12-377/Jubjub**: re-prove a "virtual"
    secp256k1 verification on a SNARK-friendly inner curve. Much
    faster proving (~30s) but requires curve-cycle infrastructure.
- Total: 1.1M–1.2M constraints if (A); 200k if (B).

We start with (A) for simplicity and ship (B) as a M-4 follow-up if
proving time is the bottleneck.

## Toolchain

- **Circuit DSL**: `circom 2.x` (mature, large library of standard
  components: SHA-256, AES, ChaCha20).
- **Proving system**: `snarkjs` for proving, `arkworks-groth16` for
  verification (the referee only verifies; provers run in extro-node).
- **Curve**: BN254 by default. Pairing-friendly, broad support.
- **WASM**: `arkworks` compiles cleanly to `wasm32-unknown-unknown`
  with no C dependencies. `snarkjs` is JS-native; provers running in
  the wallet implementor's PWA use it directly.

## Verifying-key handling

The referee loads two pre-prepared verifying keys at boot, one per
circuit. They are bundled with the binary (or loaded from a configured
path). Verification keys are public — anyone can re-run verification
against the audit log.

When circuit definitions change (e.g. (A) → (B) curve-cycle migration),
the verifying keys change too. The referee's `/v1/pubkey` endpoint
includes a `circuit_version` field (TBD addition) so wallets pin a
specific verifier version.

## Reference implementation status

| Component | Status |
|---|---|
| `Verifier` trait + `MockVerifier` | Implemented (`src/zkp.rs`) |
| `ArkworksVerifier` (gated `zkp-arkworks`) | Stubbed; verifies by accepting all proofs (M-5 placeholder) |
| Bob's circuit (.circom) | Authored in extro-node — separate project |
| Alice's circuit (.circom) | Authored in extro-node |
| Verifying-key bundling | Pending — extro-node will publish |
| Constraint-count benchmarks | Pending — `cargo bench -p referee-zkp` once circuits land |

## Security checklist

Before declaring the ZKP layer production-ready:

- [ ] Both circuits authored and audited (separate audit pass from the
  referee crate).
- [ ] Trusted setup (Powers of Tau) for BN254 used; ceremony participants
  documented.
- [ ] Verifying keys checked in at a known commit hash; the audit log
  records which key was used for each verification.
- [ ] Constraint counts measured and within budget (target <30s prove,
  <100ms verify).
- [ ] WASM verifier bundle <500KB.
- [ ] Test vectors: 100+ proofs (50 valid, 50 invalid) checked against
  the verifier in CI.
