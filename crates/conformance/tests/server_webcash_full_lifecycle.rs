//! End-to-end lifecycle test: mine → health_check → replace → health_check → burn → health_check.
//!
//! Boots a Redis container and the `webycash-server-webcash` binary, then
//! drives every state-changing endpoint and asserts each step succeeds with
//! the expected response shape.
//!
//! Skips when Docker is unavailable or the server binary hasn't been built.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use sha2::{Digest, Sha256};

const SERVER_BIN: &str = "webycash-server-webcash";

fn server_binary_path() -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    std::path::PathBuf::from(&manifest_dir)
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("debug")
        .join(SERVER_BIN)
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

struct RedisContainer {
    name: String,
    port: u16,
}

impl RedisContainer {
    fn start() -> Option<Self> {
        if !docker_available() {
            return None;
        }
        let port = pick_port();
        let name = format!("webycash-conf-redis-{}", uuid_short());
        let _ = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "-p",
                &format!("{port}:6379"),
                "redis:7-alpine",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        // Wait for Redis to be reachable.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return Some(RedisContainer { name, port });
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }
}

impl Drop for RedisContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["stop", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn uuid_short() -> String {
    let n: u64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    format!("{n:x}")
}

fn http_post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    http_send(url, "POST", Some(body))
}

fn http_get(url: &str) -> std::io::Result<(u16, String)> {
    http_send(url, "GET", None)
}

fn http_send(url: &str, method: &str, body: Option<&str>) -> std::io::Result<(u16, String)> {
    let after_scheme = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after_scheme.to_string(), "/".to_string()));

    let mut stream = std::net::TcpStream::connect(&host_port)?;
    let body_bytes = body.unwrap_or("").as_bytes();
    let req = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: {host_port}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body_bytes.len()
    );
    stream.write_all(req.as_bytes())?;
    if !body_bytes.is_empty() {
        stream.write_all(body_bytes)?;
    }
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

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// Find a nonce that produces a SHA256 with at least `bits` leading zero bits.
/// For the test we use bits=4 which is always findable in <100 tries.
fn find_preimage_nonce(template: &str, bits: u32) -> (u64, String) {
    for nonce in 0..200_000u64 {
        let preimage = template.replace("__NONCE__", &nonce.to_string());
        let hash = Sha256::digest(preimage.as_bytes());
        let lz = leading_zero_bits(&hash);
        if lz >= bits {
            return (nonce, preimage);
        }
    }
    panic!("could not find nonce satisfying difficulty bits={bits}");
}

fn leading_zero_bits(hash: &[u8]) -> u32 {
    let full_zero_bytes = hash.iter().take_while(|&&b| b == 0).count() as u32;
    hash.get(full_zero_bytes as usize)
        .map_or(0, |b| b.leading_zeros())
        + full_zero_bytes * 8
}

fn json_string_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[test]
fn full_webcash_lifecycle() {
    let bin = server_binary_path();
    if !bin.exists() {
        eprintln!(
            "skipping: {} not built. Run `cargo build -p webycash-server-webcash` first.",
            bin.display()
        );
        return;
    }
    let Some(redis) = RedisContainer::start() else {
        eprintln!("skipping: Docker unavailable or Redis failed to start");
        return;
    };

    let server_port = pick_port();
    let bind = format!("127.0.0.1:{server_port}");
    let redis_url = format!("redis://127.0.0.1:{}", redis.port);

    let mut child = Command::new(&bin)
        .env("WEBCASH_BIND_ADDR", &bind)
        .env("WEBCASH_MODE", "testnet")
        .env("WEBYCASH_DIFFICULTY", "4")
        .env("REDIS_URL", &redis_url)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server-webcash");

    // Wait for the server to bind.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(&bind).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // 1. /api/v1/target should serve immediately.
    let (status, body) = http_get(&format!("http://{bind}/api/v1/target")).expect("target");
    assert_eq!(status, 200, "target body: {body}");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("target json");
    assert_eq!(parsed["difficulty_target_bits"], 4);

    // 2. mine 1.0 webcash via /api/v1/mining_report.
    let test_secret =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let preimage_template = format!(
        r#"{{"webcash":["e1.0:secret:{test_secret}"],"subsidy":["e0.5:secret:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],"timestamp":1714003200,"difficulty":4,"nonce":__NONCE__}}"#
    );
    let (_nonce, preimage) = find_preimage_nonce(&preimage_template, 4);

    let mining_body = format!(
        r#"{{"preimage": {p}, "legalese": {{"terms": true}}}}"#,
        p = json_string_escape(&preimage)
    );
    let (status, body) =
        http_post(&format!("http://{bind}/api/v1/mining_report"), &mining_body).expect("mine");
    assert_eq!(status, 200, "mine body: {body}");
    assert!(body.contains(r#""status": "success""#), "mine: {body}");

    // 3. health_check the mined hash → spent: false.
    let public_hash = sha256_hex(test_secret);
    let hc_body = format!(r#"["e1.0:public:{public_hash}"]"#);
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &hc_body,
    )
    .expect("hc1");
    assert_eq!(status, 200, "hc1: {body}");
    assert!(body.contains(r#""spent": false"#), "hc1 expects unspent: {body}");

    // 4. /api/v1/replace: 1.0 → 0.4 + 0.6.
    let out1 = "1111111111111111111111111111111111111111111111111111111111111111";
    let out2 = "2222222222222222222222222222222222222222222222222222222222222222";
    let replace_body = format!(
        r#"{{"webcashes":["e1.0:secret:{test_secret}"],"new_webcashes":["e0.4:secret:{out1}","e0.6:secret:{out2}"],"legalese":{{"terms":true}}}}"#
    );
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/replace"),
        &replace_body,
    )
    .expect("replace");
    assert_eq!(status, 200, "replace: {body}");
    assert!(body.contains(r#""status": "success""#));

    // 5. health_check input + outputs.
    let out1_hash = sha256_hex(out1);
    let out2_hash = sha256_hex(out2);
    let hc2 = format!(
        r#"["e1.0:public:{public_hash}","e0.4:public:{out1_hash}","e0.6:public:{out2_hash}"]"#
    );
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &hc2,
    )
    .expect("hc2");
    assert_eq!(status, 200);
    // input must be spent, both outputs must be unspent
    assert!(
        body.contains(&format!(r#""e1:public:{public_hash}": {{"spent": true}}"#)),
        "hc2 input: {body}"
    );
    assert!(
        body.contains(&format!(r#""e0.4:public:{out1_hash}": {{"spent": false}}"#)),
        "hc2 out1: {body}"
    );
    assert!(
        body.contains(&format!(r#""e0.6:public:{out2_hash}": {{"spent": false}}"#)),
        "hc2 out2: {body}"
    );

    // 6. burn 0.4.
    let burn_body = format!(
        r#"{{"webcash":"e0.4:secret:{out1}","legalese":{{"terms":true}}}}"#
    );
    let (status, body) = http_post(&format!("http://{bind}/api/v1/burn"), &burn_body)
        .expect("burn");
    assert_eq!(status, 200, "burn: {body}");
    assert!(body.contains(r#""status": "success""#));

    // 7. health_check after burn: 0.4 spent, 0.6 unspent.
    let hc3 = format!(r#"["e0.4:public:{out1_hash}","e0.6:public:{out2_hash}"]"#);
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &hc3,
    )
    .expect("hc3");
    assert_eq!(status, 200);
    assert!(
        body.contains(&format!(r#""e0.4:public:{out1_hash}": {{"spent": true}}"#)),
        "hc3 out1: {body}"
    );
    assert!(
        body.contains(&format!(r#""e0.6:public:{out2_hash}": {{"spent": false}}"#)),
        "hc3 out2: {body}"
    );

    // 8. negative paths: replace with mismatched amounts must fail.
    let bad_replace = format!(
        r#"{{"webcashes":["e0.6:secret:{out2}"],"new_webcashes":["e0.5:secret:3333333333333333333333333333333333333333333333333333333333333333"],"legalese":{{"terms":true}}}}"#
    );
    let (status, _body) = http_post(&format!("http://{bind}/api/v1/replace"), &bad_replace)
        .expect("bad replace");
    assert_eq!(status, 500, "amount mismatch must 500");

    let _ = child.kill();
    let _ = child.wait();
}
