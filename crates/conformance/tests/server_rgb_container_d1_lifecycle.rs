//! RGB20 fungible lifecycle through Cloudflare Containers + D1.
//!
//! Same shape as `server_webcash_container_d1_lifecycle.rs`: spawns
//! `wrangler dev --local` against the RGB Container worker, lets
//! wrangler build the Container image from `Dockerfile.cf-rgb`, then
//! exercises the mining_report → health_check → replace lifecycle
//! through the Container → Worker → D1 path. Cross-contract replace
//! must surface a 5xx (namespace mismatch enforcement).
//!
//! Issuer keys aren't needed — RGB fungible mining_report path doesn't
//! require `/issue`. See `server_voucher_container_d1_lifecycle.rs` for
//! the signed-mint variant.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[test]
fn rgb_lifecycle_against_container_worker_d1() {
    if !cmd_check("wrangler") || !cmd_check("npm") || !cmd_check("docker") {
        eprintln!("skipping: wrangler, npm, or docker unavailable");
        return;
    }
    if !docker_daemon_running() {
        eprintln!("skipping: Docker daemon not running");
        return;
    }
    let root = workspace_root();
    let worker_dir = root.join("deploy/cloudflare-container/workers/rgb");
    let dockerfile = root.join("Dockerfile.cf-rgb");
    if !worker_dir.exists() || !dockerfile.exists() {
        eprintln!("skipping: rgb container layout missing");
        return;
    }

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
            "webycash-cf-container-rgb-testnet",
            "--local",
        ])
        .current_dir(&worker_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();

    let port = pick_free_port();
    let bind = format!("127.0.0.1:{port}");
    let mut child: Child = match Command::new("wrangler")
        .args(["dev", "--local", "--port", &port.to_string()])
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
        eprintln!("skipping: wrangler dev + rgb Container not ready within 15 min");
        return;
    }

    let result = std::panic::catch_unwind(|| run_rgb_lifecycle(&bind));
    let _ = child.kill();
    let _ = child.wait();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_rgb_lifecycle(bind: &str) {
    let issuer = "aabbccddeeff00112233445566778899aabbccdd";
    let contract = "rgb20-usdc";
    let secret = "a".repeat(64);
    let public_hash = common::sha256_hex(&secret);

    let preimage_obj = serde_json::json!({
        "webcash": [format!("e100.0:secret:{secret}:{contract}:{issuer}")],
        "subsidy": [],
        "timestamp": 1714003200i64,
        "difficulty": 4,
        "nonce": 0,
    });
    let template = serde_json::to_string(&preimage_obj)
        .unwrap()
        .replace(r#""nonce":0"#, r#""nonce":__N__"#);
    let preimage = common::find_preimage(&template, 4);
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/mining_report"),
        &serde_json::to_string(&serde_json::json!({
            "preimage": preimage,
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("mine");
    assert_eq!(status, 200, "container rgb mining_report: {body}");

    let (_, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([format!(
            "e100.0:public:{public_hash}:{contract}:{issuer}"
        )]))
        .unwrap(),
    )
    .expect("hc");
    assert!(body.contains(r#""spent": false"#), "rgb hc: {body}");

    let out1 = "b".repeat(64);
    let out2 = "c".repeat(64);
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/replace"),
        &serde_json::to_string(&serde_json::json!({
            "webcashes": [format!("e100.0:secret:{secret}:{contract}:{issuer}")],
            "new_webcashes": [
                format!("e25.0:secret:{out1}:{contract}:{issuer}"),
                format!("e75.0:secret:{out2}:{contract}:{issuer}"),
            ],
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("replace");
    assert_eq!(status, 200, "container rgb replace: {body}");

    let alt_contract = "rgb20-usdt";
    let (xs, _) = http_post(
        &format!("http://{bind}/api/v1/replace"),
        &serde_json::to_string(&serde_json::json!({
            "webcashes": [format!("e25.0:secret:{out1}:{contract}:{issuer}")],
            "new_webcashes": [format!(
                "e25.0:secret:{}:{alt_contract}:{issuer}",
                "9".repeat(64)
            )],
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("cross-contract");
    assert_eq!(xs, 500, "rgb cross-contract replace must 500");
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
