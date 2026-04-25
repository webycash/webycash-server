//! Issuer authentication — Ed25519 signature verification, nonce cache,
//! issuer registry.
//!
//! Used by RGB and Voucher server flavors for `/api/v1/issue`. Webcash
//! flavor does not depend on this crate.
//!
//! State of M3.F: Ed25519 raw-key signature verification using
//! `ed25519-dalek`. The full OpenPGP V4 cert handling (rpgp) is a
//! follow-up; current server config takes a list of `(issuer_fp,
//! pubkey_bytes)` pairs directly so the deployment can roll its own
//! key distribution while we land the PGP layer.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use webycash_asset_core::PgpFingerprint;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("issuer fingerprint not registered: {0}")]
    UnknownIssuer(String),
    #[error("invalid signature")]
    InvalidSignature,
    #[error("nonce already seen (replay)")]
    ReplayedNonce,
    #[error("malformed signature: {0}")]
    MalformedSignature(String),
    #[error("malformed pubkey: {0}")]
    MalformedPubkey(String),
}

/// Registered issuers: fingerprint → Ed25519 verifying key.
pub struct IssuerRegistry {
    keys: HashMap<String, VerifyingKey>,
}

impl Default for IssuerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IssuerRegistry {
    pub fn new() -> Self {
        Self {
            keys: HashMap::new(),
        }
    }

    /// Register an issuer from raw bytes. Fingerprint is the standard
    /// 20-byte OpenPGP V4 fingerprint when the user supplies it; in
    /// dev/testnet operators may use any unique 40-char hex identifier.
    pub fn add(&mut self, fp: &str, pubkey_bytes: &[u8]) -> Result<(), AuthError> {
        if pubkey_bytes.len() != 32 {
            return Err(AuthError::MalformedPubkey(format!(
                "expected 32 bytes, got {}",
                pubkey_bytes.len()
            )));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(pubkey_bytes);
        let key = VerifyingKey::from_bytes(&arr)
            .map_err(|e| AuthError::MalformedPubkey(e.to_string()))?;
        self.keys.insert(fp.to_lowercase(), key);
        Ok(())
    }

    /// Add issuer from hex-encoded pubkey.
    pub fn add_hex(&mut self, fp: &str, pubkey_hex: &str) -> Result<(), AuthError> {
        let bytes = hex::decode(pubkey_hex)
            .map_err(|e| AuthError::MalformedPubkey(format!("hex: {e}")))?;
        self.add(fp, &bytes)
    }

    /// Verify that `sig_bytes` is a valid Ed25519 signature of `body_bytes`
    /// produced by the issuer with fingerprint `fp`. Returns `Ok(())` on
    /// match, error otherwise.
    pub fn verify(
        &self,
        fp: &PgpFingerprint,
        body_bytes: &[u8],
        sig_bytes: &[u8],
    ) -> Result<(), AuthError> {
        let key = self
            .keys
            .get(&fp.0.to_lowercase())
            .ok_or_else(|| AuthError::UnknownIssuer(fp.0.clone()))?;
        if sig_bytes.len() != 64 {
            return Err(AuthError::MalformedSignature(format!(
                "expected 64 bytes, got {}",
                sig_bytes.len()
            )));
        }
        let mut arr = [0u8; 64];
        arr.copy_from_slice(sig_bytes);
        let sig = Signature::from_bytes(&arr);
        key.verify(body_bytes, &sig)
            .map_err(|_| AuthError::InvalidSignature)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Parse an ASCII-armored OpenPGP V4 cert, extract the primary signing
    /// key (must be Ed25519), and register it. Returns the discovered
    /// fingerprint as 40-hex-char V4 form.
    ///
    /// Requires the `openpgp` cargo feature.
    #[cfg(feature = "openpgp")]
    pub fn add_pgp_armored(&mut self, cert_armored: &str) -> Result<String, AuthError> {
        use pgp::composed::Deserializable;
        use pgp::types::{KeyDetails, PublicParams};

        let (cert, _) = pgp::composed::SignedPublicKey::from_string(cert_armored)
            .map_err(|e| AuthError::MalformedPubkey(format!("pgp parse: {e}")))?;
        let primary = &cert.primary_key;
        let pubkey_bytes: [u8; 32] = match primary.public_params() {
            PublicParams::Ed25519(p) => *p.key.as_bytes(),
            PublicParams::EdDSALegacy(_) => {
                // Legacy EdDSA-on-curve25519 stored as a curve25519 MPI; rpgp
                // already exposes the parsed VerifyingKey via the params struct
                // but the type is private — extract via debug formatting.
                // Pragmatic: ask the caller to use the modern Ed25519 (RFC 9580)
                // packet form. Most tooling has migrated.
                return Err(AuthError::MalformedPubkey(
                    "legacy EdDSA cert; please re-export with RFC 9580 Ed25519 packet"
                        .into(),
                ));
            }
            other => {
                return Err(AuthError::MalformedPubkey(format!(
                    "unsupported primary key algorithm; need Ed25519, got {other:?}"
                )));
            }
        };
        let fp_hex = hex::encode(primary.fingerprint().as_bytes());
        self.add(&fp_hex, &pubkey_bytes)?;
        Ok(fp_hex)
    }
}

/// In-memory nonce cache for replay protection.
///
/// Bounded LRU via simple counter; production deployments can swap for a
/// Redis SETEX-backed implementation when scale demands. Each issuance
/// request must carry a unique `(issuer_fp, nonce)` pair within a
/// configured TTL.
pub struct NonceCache {
    seen: Mutex<HashSet<String>>,
    max_size: usize,
}

impl Default for NonceCache {
    fn default() -> Self {
        Self::with_capacity(100_000)
    }
}

impl NonceCache {
    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            seen: Mutex::new(HashSet::new()),
            max_size,
        }
    }

    /// Returns `Ok(())` if `(fp, nonce)` is fresh; `Err(ReplayedNonce)` if
    /// already seen. Caller must hold the gate AFTER signature verification.
    pub fn check_and_insert(&self, fp: &PgpFingerprint, nonce: &str) -> Result<(), AuthError> {
        let mut seen = self
            .seen
            .lock()
            .map_err(|_| AuthError::InvalidSignature)?;
        if seen.len() >= self.max_size {
            seen.clear();
        }
        let key = format!("{}:{}", fp.0, nonce);
        if !seen.insert(key) {
            return Err(AuthError::ReplayedNonce);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn keypair() -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    #[test]
    fn verify_valid_signature() {
        let (sk, vk) = keypair();
        let mut reg = IssuerRegistry::new();
        let fp = PgpFingerprint("a".repeat(40));
        reg.add(&fp.0, vk.as_bytes()).unwrap();
        let body = b"some canonical body";
        let sig = ed25519_dalek::Signer::sign(&sk, body);
        reg.verify(&fp, body, &sig.to_bytes()).unwrap();
    }

    #[test]
    fn rejects_wrong_signature() {
        let (sk, vk) = keypair();
        let mut reg = IssuerRegistry::new();
        let fp = PgpFingerprint("a".repeat(40));
        reg.add(&fp.0, vk.as_bytes()).unwrap();
        let body = b"some canonical body";
        let sig = ed25519_dalek::Signer::sign(&sk, body);
        let tampered = b"tampered body";
        assert!(matches!(
            reg.verify(&fp, tampered, &sig.to_bytes()),
            Err(AuthError::InvalidSignature)
        ));
    }

    #[test]
    fn rejects_unregistered_issuer() {
        let (sk, _vk) = keypair();
        let reg = IssuerRegistry::new();
        let fp = PgpFingerprint("b".repeat(40));
        let body = b"x";
        let sig = ed25519_dalek::Signer::sign(&sk, body);
        assert!(matches!(
            reg.verify(&fp, body, &sig.to_bytes()),
            Err(AuthError::UnknownIssuer(_))
        ));
    }

    #[test]
    fn nonce_cache_blocks_replay() {
        let cache = NonceCache::with_capacity(10);
        let fp = PgpFingerprint("a".repeat(40));
        cache.check_and_insert(&fp, "nonce-1").unwrap();
        assert!(matches!(
            cache.check_and_insert(&fp, "nonce-1"),
            Err(AuthError::ReplayedNonce)
        ));
        // Different nonce ok
        cache.check_and_insert(&fp, "nonce-2").unwrap();
    }

    /// Round-trip: generate a V4 OpenPGP cert with Ed25519 primary key,
    /// armor it, parse via `add_pgp_armored`, sign with the key bytes
    /// extracted from the cert, and verify the signature against the
    /// registered (fp, pubkey).
    #[cfg(feature = "openpgp")]
    #[test]
    fn pgp_armored_cert_round_trip() {
        use pgp::composed::{
            EncryptionCaps, KeyType, SecretKeyParamsBuilder, SignedPublicKey,
        };
        use pgp::types::{KeyDetails as _, PlainSecretParams};
        use rand::rngs::StdRng;
        use rand::SeedableRng;

        let mut rng = StdRng::seed_from_u64(42);
        let key_params = SecretKeyParamsBuilder::default()
            .key_type(KeyType::Ed25519)
            .can_certify(true)
            .can_sign(true)
            .can_encrypt(EncryptionCaps::None)
            .primary_user_id("Test Issuer <issuer@example.org>".into())
            .passphrase(None)
            .build()
            .expect("build params");
        let signed_secret = key_params
            .generate(&mut rng)
            .expect("generate secret key");

        // Extract the raw 32-byte Ed25519 secret seed.
        let seed = signed_secret
            .primary_key
            .unlock(&"".into(), |_pub_params, plain| match plain {
                PlainSecretParams::Ed25519(k) => Ok(*k.as_bytes()),
                _ => panic!("expected Ed25519 secret"),
            })
            .expect("unlock outer")
            .expect("unlock inner");
        let dalek_sk = ed25519_dalek::SigningKey::from_bytes(&seed);

        // Armor the public side.
        let public_key = signed_secret.to_public_key();
        let armor = public_key
            .to_armored_string(None.into())
            .expect("armor public key");

        // Round-trip through the registry.
        let mut reg = IssuerRegistry::new();
        let fp_hex = reg.add_pgp_armored(&armor).expect("register");
        assert_eq!(
            fp_hex,
            hex::encode(SignedPublicKey::from(signed_secret).primary_key.fingerprint().as_bytes())
        );

        // The signature produced via ed25519-dalek (using the seed we
        // dug out of the cert) verifies against the registered cert.
        let body = b"canonical body to sign";
        let sig = ed25519_dalek::Signer::sign(&dalek_sk, body);
        reg.verify(&PgpFingerprint(fp_hex), body, &sig.to_bytes())
            .expect("verify");
    }
}
