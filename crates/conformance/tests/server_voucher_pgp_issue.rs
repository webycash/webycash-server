//! End-to-end test: operator hands the server an OpenPGP V4 armored
//! cert (Ed25519 primary), the server registers the cert's discovered
//! fingerprint and Ed25519 verifying key, then `/api/v1/issue` accepts
//! signatures by the matching Ed25519 secret key.
//!
//! This is the production flow: an issuer registers their PGP cert with
//! the operator out-of-band; their wallet signs every issuance request
//! with the same Ed25519 key the cert advertises.

mod common;
use common::*;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use ed25519_dalek::Signer;
use pgp::composed::{EncryptionCaps, KeyType, SecretKeyParamsBuilder};
use pgp::types::{KeyDetails as _, PlainSecretParams};
use rand::rngs::StdRng;
use rand::SeedableRng;

#[test]
fn voucher_pgp_armored_issue_end_to_end() {
    let bin_name = "webycash-server-voucher";
    let bin = bin_path(bin_name);
    if !bin.exists() {
        eprintln!("skipping: {} not built", bin.display());
        return;
    }
    if !docker_avail() {
        eprintln!("skipping: Docker unavailable");
        return;
    }

    // 1. Generate an OpenPGP V4 cert with Ed25519 primary key.
    let mut rng = StdRng::seed_from_u64(7);
    let key_params = SecretKeyParamsBuilder::default()
        .key_type(KeyType::Ed25519)
        .can_certify(true)
        .can_sign(true)
        .can_encrypt(EncryptionCaps::None)
        .primary_user_id("Voucher Issuer <issuer@example.org>".into())
        .passphrase(None)
        .build()
        .expect("build params");
    let signed_secret = key_params.generate(&mut rng).expect("generate cert");

    let seed = signed_secret
        .primary_key
        .unlock(&"".into(), |_p, plain| match plain {
            PlainSecretParams::Ed25519(k) => Ok(*k.as_bytes()),
            _ => panic!("expected Ed25519 secret"),
        })
        .expect("unlock outer")
        .expect("unlock inner");
    let dalek_sk = ed25519_dalek::SigningKey::from_bytes(&seed);
    let fp = hex::encode(signed_secret.primary_key.fingerprint().as_bytes());

    let public_key = signed_secret.to_public_key();
    let armor = public_key
        .to_armored_string(None.into())
        .expect("armor public key");

    // 2. Write the armored cert to a tempfile and start the voucher binary
    //    pointing WEBYCASH_ISSUER_PGP_CERTS at it.
    let cert_file = tempfile::NamedTempFile::new().expect("tmpfile");
    write!(cert_file.as_file(), "{armor}").expect("write cert");

    let redis_port = ephemeral_port();
    let redis_name = format!("conf-redis-pgp-{}", short_id());
    Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &redis_name,
            "-p",
            &format!("{redis_port}:6379"),
            "redis:7-alpine",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("docker run");
    if !await_tcp("127.0.0.1", redis_port, Duration::from_secs(20)) {
        let _ = stop(&redis_name);
        panic!("redis didn't start");
    }

    let server_port = ephemeral_port();
    let bind = format!("127.0.0.1:{server_port}");
    let mut child = Command::new(&bin)
        .env("WEBCASH_BIND_ADDR", &bind)
        .env("WEBCASH_MODE", "testnet")
        .env("REDIS_URL", format!("redis://127.0.0.1:{redis_port}"))
        .env("WEBYCASH_ISSUER_PGP_CERTS", cert_file.path())
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn voucher");
    if !await_tcp("127.0.0.1", server_port, Duration::from_secs(15)) {
        let _ = child.kill();
        let mut stderr = String::new();
        if let Some(mut e) = child.stderr.take() {
            use std::io::Read;
            let _ = e.read_to_string(&mut stderr);
        }
        let _ = child.wait();
        let _ = stop(&redis_name);
        panic!("server didn't bind. stderr:\n{stderr}");
    }

    let result = std::panic::catch_unwind(|| {
        // 3. /issue with a signature produced by the same Ed25519 key.
        let secret = "9".repeat(64);
        let public_hash = sha256_hex(&secret);
        let contract = "credits-pgp";
        let body_obj = serde_json::json!({
            "issuer_fp": fp,
            "outputs": [format!("e25.0:secret:{secret}:{contract}:{fp}")],
            "nonce": "pgp-nonce-1",
            "ts": 1_714_003_200_u64,
            "legalese": {"terms": true},
        });
        let body = serde_json::to_vec(&body_obj).unwrap();
        let sig = dalek_sk.sign(&body);
        let sig_hex = hex::encode(sig.to_bytes());

        let (status, resp) = post_with_header(
            &format!("http://{bind}/api/v1/issue"),
            std::str::from_utf8(&body).unwrap(),
            ("X-Issuer-Signature", &sig_hex),
        )
        .expect("issue");
        assert_eq!(status, 200, "issue: {resp}");
        assert!(resp.contains(r#""status": "success""#));

        // Confirm the issued voucher is unspent under the registered (fp, contract).
        let (hc_status, hc_body) = post_no_header(
            &format!("http://{bind}/api/v1/health_check"),
            &serde_json::to_string(&serde_json::json!([format!(
                "e25.0:public:{public_hash}:{contract}:{fp}"
            )]))
            .unwrap(),
        )
        .expect("hc");
        assert_eq!(hc_status, 200);
        assert!(hc_body.contains(r#""spent": false"#), "hc: {hc_body}");

        // Tampered body must fail signature check (same nonce; the body
        // hash differs so the signature can't verify).
        let bad = body_obj.to_string().replace("25.0", "2500.0");
        let (tamper_status, _t) = post_with_header(
            &format!("http://{bind}/api/v1/issue"),
            &bad,
            ("X-Issuer-Signature", &sig_hex),
        )
        .expect("tamper");
        assert_eq!(tamper_status, 500);
    });

    let _ = child.kill();
    let _ = child.wait();
    let _ = stop(&redis_name);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

// ─── local helpers ─────────────────────────────────────────────────────

fn bin_path(b: &str) -> std::path::PathBuf {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    std::path::PathBuf::from(&manifest)
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("debug")
        .join(b)
}
fn ephemeral_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    s.local_addr().unwrap().port()
}
fn docker_avail() -> bool {
    Command::new("docker")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
fn stop(name: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("docker")
        .args(["stop", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}
fn short_id() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    )
}
fn await_tcp(host: &str, port: u16, max: Duration) -> bool {
    let deadline = std::time::Instant::now() + max;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect((host, port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
fn post_with_header(url: &str, body: &str, header: (&str, &str)) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".into()));
    let mut s = std::net::TcpStream::connect(&host_port)?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}: {}\r\nConnection: close\r\n\r\n",
        body.len(),
        header.0,
        header.1,
    );
    s.write_all(req.as_bytes())?;
    s.write_all(body.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    Ok((status, text[body_start..].to_string()))
}
fn post_no_header(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    post_with_header(url, body, ("X-Test", ""))
}
