//! RGB-Fungible full lifecycle against DynamoDB Local.

mod common;
use common::*;

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[test]
fn rgb_lifecycle_against_dynamodb_local() {
    let bin = bin_path("webycash-server-rgb");
    if !bin.exists() || !docker_avail() {
        eprintln!("skipping");
        return;
    }
    let ddb_port = ephemeral_port();
    let ddb_name = format!("conf-ddb-rgb-{}", short_id());
    Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "--name",
            &ddb_name,
            "-p",
            &format!("{ddb_port}:8000"),
            "amazon/dynamodb-local:latest",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("ddb run");
    // TCP-ready isn't enough; the JVM-backed server takes a moment longer
    // before its API responds reliably. Probe the API too.
    let ddb_url = format!("http://127.0.0.1:{ddb_port}");
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", ddb_port)).is_ok()
            && probe(&ddb_url).is_ok()
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if !ready {
        let _ = stop(&ddb_name);
        return;
    }

    let server_port = ephemeral_port();
    let bind = format!("127.0.0.1:{server_port}");
    let mut child: Child = Command::new(&bin)
        .env("WEBCASH_BIND_ADDR", &bind)
        .env("WEBCASH_MODE", "testnet")
        .env("WEBYCASH_DIFFICULTY", "4")
        .env("WEBCASH_DB_BACKEND", "dynamodb")
        .env("DYNAMODB_ENDPOINT", format!("http://127.0.0.1:{ddb_port}"))
        .env("AWS_ACCESS_KEY_ID", "fake")
        .env("AWS_SECRET_ACCESS_KEY", "fake")
        .env("AWS_REGION", "us-east-1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn rgb");
    if !await_tcp("127.0.0.1", server_port, Duration::from_secs(20)) {
        let _ = child.kill();
        let _ = child.wait();
        let _ = stop(&ddb_name);
        panic!("server didn't bind");
    }

    let result = std::panic::catch_unwind(|| run_rgb_lifecycle(&bind));
    let _ = child.kill();
    let _ = child.wait();
    let _ = stop(&ddb_name);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_rgb_lifecycle(bind: &str) {
    let issuer = "aabbccddeeff00112233445566778899aabbccdd";
    let contract = "rgb20-usdc";
    let secret = "a".repeat(64);
    let public_hash = sha256_hex(&secret);

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
    let preimage = find_preimage(&template, 4);
    let (status, _) = post(
        &format!("http://{bind}/api/v1/mining_report"),
        &serde_json::to_string(&serde_json::json!({
            "preimage": preimage, "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(status, 200);

    let (_, body) = post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([format!(
            "e100.0:public:{public_hash}:{contract}:{issuer}"
        )]))
        .unwrap(),
    )
    .unwrap();
    assert!(body.contains(r#""spent": false"#), "{body}");

    let out1 = "b".repeat(64);
    let out2 = "c".repeat(64);
    let out1h = sha256_hex(&out1);
    let out2h = sha256_hex(&out2);
    let (status, _) = post(
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
    .unwrap();
    assert_eq!(status, 200);

    let (_, body) = post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([
            format!("e100.0:public:{public_hash}:{contract}:{issuer}"),
            format!("e25.0:public:{out1h}:{contract}:{issuer}"),
            format!("e75.0:public:{out2h}:{contract}:{issuer}"),
        ]))
        .unwrap(),
    )
    .unwrap();
    assert!(
        body.contains(&format!(
            r#""e100:public:{public_hash}:{contract}:{issuer}": {{"spent": true}}"#
        )),
        "input not spent: {body}"
    );
    assert!(
        body.contains(&format!(
            r#""e25:public:{out1h}:{contract}:{issuer}": {{"spent": false}}"#
        )),
        "out1 not unspent: {body}"
    );

    // Cross-contract replace must fail.
    let alt_contract = "rgb20-usdt";
    let (xn, _) = post(
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
    .unwrap();
    assert_eq!(xn, 500);
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

fn probe(url: &str) -> std::io::Result<()> {
    let after = url.strip_prefix("http://").unwrap_or(url);
    let host_port = after.split('/').next().unwrap_or(after);
    let mut s = std::net::TcpStream::connect(host_port)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut buf = [0u8; 64];
    let _ = s.read(&mut buf)?;
    Ok(())
}
fn post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".into()));
    let mut s = std::net::TcpStream::connect(&host_port)?;
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: {host_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
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
