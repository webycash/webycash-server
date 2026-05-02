//! Webcash full lifecycle through DynamoDB Local.
//!
//! Mirrors `server_webcash_full_lifecycle.rs` but switches the storage
//! backend to DynamoDB Local via `WEBCASH_DB_BACKEND=dynamodb`. Same
//! protocol, different backend — proves the asset-gated server speaks
//! identical wire format regardless of where it stores its ledger.

mod common;

use common::*;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[test]
fn webcash_lifecycle_against_dynamodb_local() {
    let bin_name = "webycash-server-webcash";
    let bin = binary_path_for(bin_name);
    if !bin.exists() {
        eprintln!("skipping: {} not built", bin.display());
        return;
    }
    if !docker_check() {
        eprintln!("skipping: Docker unavailable");
        return;
    }

    // 1. Start DynamoDB Local in Docker.
    let ddb_port = pick_free_port();
    let ddb_name = format!("webycash-conf-ddb-{}", uuid_short_str());
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
        .expect("docker run dynamodb-local");
    // Wait for DynamoDB Local: it can take 5+ seconds for the JVM to be ready.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let ddb_url = format!("http://127.0.0.1:{ddb_port}");
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", ddb_port)).is_ok() {
            // Issue a quick HTTP request to confirm the API responds.
            if poll_http(&ddb_url).is_ok() {
                ready = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if !ready {
        let _ = stop_docker(&ddb_name);
        eprintln!("skipping: DynamoDB Local not ready within 30s");
        return;
    }

    // 2. Boot server-webcash against DynamoDB Local.
    let server_port = pick_free_port();
    let bind = format!("127.0.0.1:{server_port}");
    let mut child: Child = Command::new(&bin)
        .env("WEBCASH_BIND_ADDR", &bind)
        .env("WEBCASH_MODE", "testnet")
        .env("WEBYCASH_DIFFICULTY", "4")
        .env("WEBCASH_DB_BACKEND", "dynamodb")
        .env("DYNAMODB_ENDPOINT", &ddb_url)
        .env("AWS_ACCESS_KEY_ID", "fake")
        .env("AWS_SECRET_ACCESS_KEY", "fake")
        .env("AWS_REGION", "us-east-1")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server-webcash");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(&bind).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let lifecycle_result = std::panic::catch_unwind(|| {
        run_lifecycle(&bind);
    });

    let _ = child.kill();
    let _ = child.wait();
    let _ = stop_docker(&ddb_name);

    if let Err(e) = lifecycle_result {
        std::panic::resume_unwind(e);
    }
}

fn run_lifecycle(bind: &str) {
    let secret = "a".repeat(64);
    let public_hash = sha256_hex(&secret);

    // mine 1.0 webcash
    let preimage_obj = serde_json::json!({
        "webcash": [format!("e1.0:secret:{secret}")],
        "subsidy": [format!("e0.5:secret:{}", "b".repeat(64))],
        "timestamp": 1714003200i64,
        "difficulty": 4,
        "nonce": 0,
    });
    let template = serde_json::to_string(&preimage_obj)
        .unwrap()
        .replace(r#""nonce":0"#, r#""nonce":__N__"#);
    let preimage = find_preimage(&template, 4);
    let (status, _) = http_post(
        &format!("http://{bind}/api/v1/mining_report"),
        &serde_json::to_string(&serde_json::json!({
            "preimage": preimage,
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("mine");
    assert_eq!(status, 200);

    // health_check after mine
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([format!(
            "e1.0:public:{public_hash}"
        )]))
        .unwrap(),
    )
    .expect("hc1");
    assert_eq!(status, 200);
    assert!(body.contains(r#""spent": false"#), "hc1: {body}");

    // replace 1.0 → 0.4 + 0.6
    let out1 = "1".repeat(64);
    let out2 = "2".repeat(64);
    let out1_hash = sha256_hex(&out1);
    let out2_hash = sha256_hex(&out2);
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/replace"),
        &serde_json::to_string(&serde_json::json!({
            "webcashes": [format!("e1.0:secret:{secret}")],
            "new_webcashes": [
                format!("e0.4:secret:{out1}"),
                format!("e0.6:secret:{out2}"),
            ],
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("replace");
    assert_eq!(status, 200, "replace: {body}");

    // health_check after replace: input spent, outputs unspent
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([
            format!("e1.0:public:{public_hash}"),
            format!("e0.4:public:{out1_hash}"),
            format!("e0.6:public:{out2_hash}"),
        ]))
        .unwrap(),
    )
    .expect("hc2");
    assert_eq!(status, 200);
    assert!(
        body.contains(&format!(
            r#""e1:public:{public_hash}": {{"spent": true, "amount": null}}"#
        )),
        "hc2 input: {body}"
    );
    assert!(
        body.contains(&format!(
            r#""e0.4:public:{out1_hash}": {{"spent": false, "amount": "0.4"}}"#
        )),
        "hc2 out1: {body}"
    );

    // burn 0.4
    let (status, _) = http_post(
        &format!("http://{bind}/api/v1/burn"),
        &serde_json::to_string(&serde_json::json!({
            "webcash": format!("e0.4:secret:{out1}"),
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("burn");
    assert_eq!(status, 200);

    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([
            format!("e0.4:public:{out1_hash}"),
            format!("e0.6:public:{out2_hash}"),
        ]))
        .unwrap(),
    )
    .expect("hc3");
    assert_eq!(status, 200);
    assert!(
        body.contains(&format!(
            r#""e0.4:public:{out1_hash}": {{"spent": true, "amount": null}}"#
        )),
        "hc3 burn: {body}"
    );
}

// Local helpers (since `common::TestHarness` only handles Redis stacks).

fn binary_path_for(bin: &str) -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    std::path::PathBuf::from(&manifest_dir)
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("debug")
        .join(bin)
}

fn pick_free_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = s.local_addr().expect("addr").port();
    drop(s);
    p
}

fn docker_check() -> bool {
    Command::new("docker")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn stop_docker(name: &str) -> std::io::Result<std::process::ExitStatus> {
    Command::new("docker")
        .args(["stop", name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
}

fn uuid_short_str() -> String {
    let n: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    format!("{n:x}")
}

fn poll_http(url: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let host_port = after.split('/').next().unwrap_or(after);
    let mut s = std::net::TcpStream::connect(host_port)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut buf = [0u8; 64];
    let _ = s.read(&mut buf)?;
    Ok(())
}

fn http_post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".to_string()));
    let mut stream = std::net::TcpStream::connect(&host_port)?;
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
