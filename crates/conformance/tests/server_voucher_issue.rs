//! End-to-end test for `/api/v1/issue`: signed operator mint that bypasses
//! mining_report. Verifies Ed25519 signature, nonce replay protection, and
//! namespace consistency.

mod common;

use common::*;

use ed25519_dalek::{Signer, SigningKey};
use std::process::{Command, Stdio};
use std::time::Duration;

#[test]
fn voucher_signed_issue_end_to_end() {
    let bin_name = "webycash-server-voucher";
    let bin = binary_path(bin_name);
    if !bin.exists() {
        eprintln!("skipping: {} not built", bin.display());
        return;
    }
    if !docker_available() {
        eprintln!("skipping: Docker unavailable");
        return;
    }

    // Generate a deterministic keypair for the test.
    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let vk = sk.verifying_key();
    let pubkey_hex = hex::encode(vk.as_bytes());
    // Use the public key's first 20 bytes as a synthetic fingerprint.
    let fp = hex::encode(&vk.as_bytes()[..20]);

    // Spin up Redis + server with this issuer registered via env.
    let redis_port = pick_port();
    let redis_name = format!("webycash-conf-redis-issue-{}", uuid_short());
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
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", redis_port)).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let server_port = pick_port();
    let bind = format!("127.0.0.1:{server_port}");
    let issuer_env = format!("{fp}:{pubkey_hex}");
    let mut child = Command::new(&bin)
        .env("WEBCASH_BIND_ADDR", &bind)
        .env("WEBCASH_MODE", "testnet")
        .env("REDIS_URL", format!("redis://127.0.0.1:{redis_port}"))
        .env("WEBYCASH_ISSUERS", &issuer_env)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(&bind).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Build an /issue request: 1 output of 50.0 credits.
    let secret = "1".repeat(64);
    let public_hash = sha256_hex(&secret);
    let contract = "credits-q1";
    let body_obj = serde_json::json!({
        "issuer_fp": fp,
        "outputs": [format!("e50.0:secret:{secret}:{contract}:{fp}")],
        "nonce": "test-nonce-1",
        "ts": 1_714_003_200_u64,
        "legalese": {"terms": true},
    });
    let body = serde_json::to_vec(&body_obj).unwrap();
    let signature = sk.sign(&body);
    let sig_hex = hex::encode(signature.to_bytes());

    let (status, resp_body) = http_post_with_header(
        &format!("http://{bind}/api/v1/issue"),
        std::str::from_utf8(&body).unwrap(),
        ("X-Issuer-Signature", &sig_hex),
    )
    .expect("issue request");
    assert_eq!(status, 200, "issue: {resp_body}");
    assert!(resp_body.contains(r#""status": "success""#));

    // Verify the issued voucher is now visible.
    let (hc_status, hc_body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([format!(
            "e50.0:public:{public_hash}:{contract}:{fp}"
        )]))
        .unwrap(),
    )
    .expect("hc");
    assert_eq!(hc_status, 200);
    assert!(hc_body.contains(r#""spent": false"#), "hc: {hc_body}");

    // Replaying the same nonce must fail.
    let (replay_status, replay_body) = http_post_with_header(
        &format!("http://{bind}/api/v1/issue"),
        std::str::from_utf8(&body).unwrap(),
        ("X-Issuer-Signature", &sig_hex),
    )
    .expect("replay");
    assert_eq!(replay_status, 500, "replay: {replay_body}");

    // Tampered body must fail signature check.
    let bad_body = body_obj.to_string().replace("50.0", "5000.0");
    let (tamper_status, _tamper_body) = http_post_with_header(
        &format!("http://{bind}/api/v1/issue"),
        &bad_body,
        ("X-Issuer-Signature", &sig_hex),
    )
    .expect("tamper");
    assert_eq!(tamper_status, 500);

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();
    let _ = Command::new("docker")
        .args(["stop", &redis_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// Local helpers (specific to this test; the common module's helpers don't
// expose header-passing yet).
fn http_post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    http_post_with_header(url, body, ("X-Test", ""))
}

fn http_post_with_header(
    url: &str,
    body: &str,
    extra: (&str, &str),
) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after_scheme = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after_scheme.to_string(), "/".to_string()));
    let mut stream = std::net::TcpStream::connect(&host_port)?;
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
    stream.write_all(req.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status_line = text.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    Ok((status, text[body_start..].to_string()))
}

fn binary_path(bin: &str) -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    std::path::PathBuf::from(&manifest_dir)
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("debug")
        .join(bin)
}

fn pick_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = s.local_addr().expect("addr").port();
    drop(s);
    p
}

fn docker_available() -> bool {
    Command::new("docker")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn uuid_short() -> String {
    let n: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    format!("{n:x}")
}
