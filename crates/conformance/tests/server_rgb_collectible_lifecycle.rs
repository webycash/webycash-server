//! RGB21 Collectible (NFT) end-to-end lifecycle.
//!
//! Boots Redis + server-rgb-collectible. Drives:
//!   - `/api/v1/issue` (operator-signed mint of an NFT)
//!   - `/api/v1/health_check` (mints unspent)
//!   - `/api/v1/transfer` (1:1 ownership move; same namespace)
//!   - `/api/v1/health_check` (input spent, output unspent)
//!   - `/api/v1/burn_collectible`
//!   - cross-contract transfer rejected

mod common;
use common::*;

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};

#[test]
fn rgb_collectible_full_lifecycle() {
    let bin = bin_path("webycash-server-rgb-collectible");
    if !bin.exists() || !docker_avail() {
        eprintln!("skipping");
        return;
    }
    let redis_port = ephemeral_port();
    let redis_name = format!("conf-redis-coll-{}", short_id());
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
        .expect("redis run");
    if !await_tcp("127.0.0.1", redis_port, Duration::from_secs(15)) {
        let _ = stop(&redis_name);
        return;
    }

    // Generate Ed25519 issuer keypair.
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let vk = sk.verifying_key();
    let pubkey_hex = hex::encode(vk.as_bytes());
    // Use first 20 bytes of the pubkey as a synthetic 40-hex fingerprint.
    let issuer = hex::encode(&vk.as_bytes()[..20]);

    let server_port = ephemeral_port();
    let bind = format!("127.0.0.1:{server_port}");
    let issuers_env = format!("{issuer}:{pubkey_hex}");
    let mut child: Child = Command::new(&bin)
        .env("WEBCASH_BIND_ADDR", &bind)
        .env("WEBCASH_MODE", "testnet")
        .env("REDIS_URL", format!("redis://127.0.0.1:{redis_port}"))
        .env("WEBYCASH_ISSUERS", &issuers_env)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    if !await_tcp("127.0.0.1", server_port, Duration::from_secs(8)) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stop(&redis_name);
        panic!("server didn't bind");
    }

    let result = std::panic::catch_unwind(|| run_lifecycle(&bind, &sk, &issuer));
    let _ = child.kill();
    let _ = child.wait();
    let _ = stop(&redis_name);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_lifecycle(bind: &str, sk: &SigningKey, issuer: &str) {
    let contract = "art-collection-1";
    let nft_secret = "a".repeat(64);
    let nft_hash = sha256_hex(&nft_secret);

    // 1. /api/v1/issue — sign and mint a single NFT.
    let body_obj = serde_json::json!({
        "issuer_fp": issuer,
        "outputs": [format!("secret:{nft_secret}:{contract}:{issuer}")],
        "nonce": "issue-1",
        "ts": 1714003200_u64,
        "legalese": {"terms": true},
    });
    let body = serde_json::to_vec(&body_obj).unwrap();
    let sig = sk.sign(&body);
    let sig_hex = hex::encode(sig.to_bytes());
    let (status, resp_body) = post_with_header(
        &format!("http://{bind}/api/v1/issue"),
        std::str::from_utf8(&body).unwrap(),
        ("X-Issuer-Signature", &sig_hex),
    )
    .expect("issue");
    assert_eq!(status, 200, "issue: {resp_body}");

    // 2. health_check — NFT must show unspent.
    let public_token = format!("public:{nft_hash}:{contract}:{issuer}");
    let (status, body) = post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([public_token])).unwrap(),
    )
    .expect("hc1");
    assert_eq!(status, 200);
    assert!(body.contains(r#""spent": false"#), "hc1: {body}");

    // 3. /api/v1/replace — 1:1 ownership replace within same namespace.
    //    The server always replaces secrets; non-splittable just constrains
    //    arity to 1 input → 1 output.
    let new_secret = "b".repeat(64);
    let new_hash = sha256_hex(&new_secret);
    let (status, body) = post(
        &format!("http://{bind}/api/v1/replace"),
        &serde_json::to_string(&serde_json::json!({
            "webcashes": [format!("secret:{nft_secret}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{new_secret}:{contract}:{issuer}")],
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("replace");
    assert_eq!(status, 200, "replace: {body}");
    assert!(body.contains(r#""status": "success""#));

    // 4. health_check — old hash spent, new hash unspent.
    let new_public = format!("public:{new_hash}:{contract}:{issuer}");
    let (_, body) = post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([
            public_token.clone(),
            new_public.clone(),
        ]))
        .unwrap(),
    )
    .expect("hc2");
    assert!(
        body.contains(&format!(r#""{public_token}": {{"spent": true}}"#)),
        "hc2 input: {body}"
    );
    assert!(
        body.contains(&format!(r#""{new_public}": {{"spent": false}}"#)),
        "hc2 output: {body}"
    );

    // 5. Cross-namespace replace must fail.
    let alt_contract = "different-collection";
    let (xn, _) = post(
        &format!("http://{bind}/api/v1/replace"),
        &serde_json::to_string(&serde_json::json!({
            "webcashes": [format!("secret:{new_secret}:{contract}:{issuer}")],
            "new_webcashes": [format!("secret:{}:{alt_contract}:{issuer}", "9".repeat(64))],
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("xn");
    assert_eq!(xn, 500, "cross-contract must 500");

    // 6. /api/v1/burn (non-splittable variant).
    let (status, _) = post(
        &format!("http://{bind}/api/v1/burn"),
        &serde_json::to_string(&serde_json::json!({
            "webcash": format!("secret:{new_secret}:{contract}:{issuer}"),
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("burn");
    assert_eq!(status, 200);

    let (_, body) = post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([new_public.clone()])).unwrap(),
    )
    .expect("hc3");
    assert!(
        body.contains(&format!(r#""{new_public}": {{"spent": true}}"#)),
        "hc3 burn: {body}"
    );
}

fn bin_path(b: &str) -> std::path::PathBuf {
    let m = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    std::path::PathBuf::from(&m)
        .ancestors()
        .nth(2)
        .unwrap()
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
fn post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    post_with_header(url, body, ("X-Test", ""))
}
fn post_with_header(url: &str, body: &str, extra: (&str, &str)) -> std::io::Result<(u16, String)> {
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".into()));
    let mut s = std::net::TcpStream::connect(&host_port)?;
    let extra_hdr = if extra.1.is_empty() {
        String::new()
    } else {
        format!("{}: {}\r\n", extra.0, extra.1)
    };
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        body.len(),
        extra_hdr,
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
