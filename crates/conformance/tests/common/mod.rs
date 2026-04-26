//! Shared test harness for server-{webcash,rgb,voucher} integration tests.
//!
//! Each integration-test binary `mod common;` this file privately, so cargo
//! lints anything it doesn't itself reference — and not every binary uses
//! every helper. Suppress dead-code warnings at the module scope rather
//! than peppering each item.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub struct TestHarness {
    child: Child,
    pub bind: String,
    _redis_name: String,
}

impl TestHarness {
    /// Start a Redis container and a server-{flavor} child process.
    /// Returns `None` if Docker isn't available or the binary isn't built.
    pub fn start(bin_name: &str) -> Option<TestHarness> {
        let bin = binary_path(bin_name);
        if !bin.exists() {
            eprintln!("skipping: {} not built", bin.display());
            return None;
        }
        if !docker_available() {
            eprintln!("skipping: Docker unavailable");
            return None;
        }
        let redis_port = pick_port();
        let redis_name = format!("webycash-conf-redis-{}-{}", bin_name, uuid_short());
        let r = Command::new("docker")
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
            .ok()?;
        if !r.success() {
            return None;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", redis_port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let server_port = pick_port();
        let bind = format!("127.0.0.1:{server_port}");
        let child = Command::new(&bin)
            .env("WEBCASH_BIND_ADDR", &bind)
            .env("WEBCASH_MODE", "testnet")
            .env("WEBYCASH_DIFFICULTY", "4")
            .env("REDIS_URL", format!("redis://127.0.0.1:{redis_port}"))
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Wait for the server to bind.
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(&bind).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(TestHarness {
            child,
            bind,
            _redis_name: redis_name,
        })
    }

    pub fn post(&self, path: &str, body: serde_json::Value) -> std::io::Result<(u16, String)> {
        let body = serde_json::to_string(&body).unwrap();
        http_send(&format!("http://{}{}", self.bind, path), "POST", Some(&body))
    }

    #[allow(dead_code)]
    pub fn get(&self, path: &str) -> std::io::Result<(u16, String)> {
        http_send(&format!("http://{}{}", self.bind, path), "GET", None)
    }
}

impl Drop for TestHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Command::new("docker")
            .args(["stop", &self._redis_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
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

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn find_preimage(template_with_nonce_marker: &str, bits: u32) -> String {
    for nonce in 0..200_000u64 {
        let preimage = template_with_nonce_marker.replace("__N__", &nonce.to_string());
        let lz = leading_zero_bits(&Sha256::digest(preimage.as_bytes()));
        if lz >= bits {
            return preimage;
        }
    }
    panic!("could not find preimage for difficulty {bits}");
}

fn leading_zero_bits(hash: &[u8]) -> u32 {
    let full_zero_bytes = hash.iter().take_while(|&&b| b == 0).count() as u32;
    hash.get(full_zero_bytes as usize)
        .map_or(0, |b| b.leading_zeros())
        + full_zero_bytes * 8
}

fn http_send(
    url: &str,
    method: &str,
    body: Option<&str>,
) -> std::io::Result<(u16, String)> {
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
