//! Voucher full lifecycle through Cloudflare Containers + D1.
//!
//! Mirrors `server_voucher_d1_lifecycle.rs` (the wasm32 variant) but the
//! native binary runs inside a Cloudflare Container instead. Issuer keys
//! flow through wrangler's `--var WEBYCASH_ISSUERS:...` into the Worker
//! [vars] and from there into the Container's start-time env.

mod common;

use ed25519_dalek::{Signer, SigningKey};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[test]
fn voucher_lifecycle_against_container_worker_d1() {
    if !cmd_check("wrangler") || !cmd_check("npm") || !cmd_check("docker") {
        eprintln!("skipping: wrangler, npm, or docker unavailable");
        return;
    }
    if !docker_daemon_running() {
        eprintln!("skipping: Docker daemon not running");
        return;
    }
    let root = workspace_root();
    let worker_dir = root.join("deploy/cloudflare-container/workers/voucher");
    let dockerfile = root.join("Dockerfile.cf-voucher");
    if !worker_dir.exists() || !dockerfile.exists() {
        eprintln!("skipping: voucher container layout missing");
        return;
    }

    let sk = SigningKey::from_bytes(&[7u8; 32]);
    let vk = sk.verifying_key();
    let pubkey_hex = hex::encode(vk.as_bytes());
    let fp = hex::encode(&vk.as_bytes()[..20]);
    let issuers = format!("{fp}:{pubkey_hex}");

    let _ = Command::new("npm")
        .args(["install", "--no-audit", "--no-fund", "--silent"])
        .current_dir(&worker_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();
    let _ = Command::new("wrangler")
        .args([
            "d1",
            "migrations",
            "apply",
            "webycash-cf-container-voucher-testnet",
            "--local",
        ])
        .current_dir(&worker_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();

    let port = pick_free_port();
    let bind = format!("127.0.0.1:{port}");
    let var_arg = format!("WEBYCASH_ISSUERS:{issuers}");
    let mut child: Child = match Command::new("wrangler")
        .args([
            "dev",
            "--local",
            "--port",
            &port.to_string(),
            "--var",
            &var_arg,
        ])
        .current_dir(&worker_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: wrangler dev spawn failed: {e}");
            return;
        }
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(900);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(&bind).is_ok()
            && http_get(&format!("http://{bind}/api/v1/target")).is_ok()
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("skipping: wrangler dev + voucher Container not ready within 15 min");
        return;
    }

    let result = std::panic::catch_unwind(|| run_voucher_lifecycle(&bind, &sk, &fp));
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_voucher_lifecycle(bind: &str, sk: &SigningKey, fp: &str) {
    let secret = "1".repeat(64);
    let public_hash = common::sha256_hex(&secret);
    let contract = "credits-q1";

    let body_obj = serde_json::json!({
        "issuer_fp": fp,
        "outputs": [format!("e50.0:secret:{secret}:{contract}:{fp}")],
        "nonce": "voucher-container-d1-nonce-1",
        "ts": 1_714_003_200_u64,
        "legalese": {"terms": true},
    });
    let body = serde_json::to_vec(&body_obj).unwrap();
    let signature = sk.sign(&body);
    let sig_hex = hex::encode(signature.to_bytes());

    let (status, resp) = http_post_with_header(
        &format!("http://{bind}/api/v1/issue"),
        std::str::from_utf8(&body).unwrap(),
        ("X-Issuer-Signature", &sig_hex),
    )
    .expect("issue");
    assert_eq!(status, 200, "container voucher issue: {resp}");

    let (hc_status, hc_body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([format!(
            "e50.0:public:{public_hash}:{contract}:{fp}"
        )]))
        .unwrap(),
    )
    .expect("hc");
    assert_eq!(hc_status, 200);
    assert!(
        hc_body.contains(r#""spent": false"#),
        "container voucher hc: {hc_body}"
    );

    let out1 = "2".repeat(64);
    let out2 = "3".repeat(64);
    let (rstatus, rbody) = http_post(
        &format!("http://{bind}/api/v1/replace"),
        &serde_json::to_string(&serde_json::json!({
            "webcashes": [format!("e50.0:secret:{secret}:{contract}:{fp}")],
            "new_webcashes": [
                format!("e30.0:secret:{out1}:{contract}:{fp}"),
                format!("e20.0:secret:{out2}:{contract}:{fp}"),
            ],
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("replace");
    assert_eq!(rstatus, 200, "container voucher replace: {rbody}");

    // Replay protection.
    let (replay_status, replay_body) = http_post_with_header(
        &format!("http://{bind}/api/v1/issue"),
        std::str::from_utf8(&body).unwrap(),
        ("X-Issuer-Signature", &sig_hex),
    )
    .expect("issue replay");
    assert_ne!(
        replay_status, 200,
        "container voucher nonce replay must be rejected: {replay_body}"
    );
}

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    std::path::PathBuf::from(&manifest_dir)
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn pick_free_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = s.local_addr().expect("addr").port();
    drop(s);
    p
}

fn cmd_check(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn docker_daemon_running() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn http_get(url: &str) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".to_string()));
    let mut stream = std::net::TcpStream::connect(&host_port)?;
    stream.set_read_timeout(Some(Duration::from_secs(8)))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes())?;
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

fn http_post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".to_string()));
    let mut stream = std::net::TcpStream::connect(&host_port)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
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

fn http_post_with_header(
    url: &str,
    body: &str,
    header: (&str, &str),
) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".to_string()));
    let mut stream = std::net::TcpStream::connect(&host_port)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\n{}: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        header.0,
        header.1,
        body.len()
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
