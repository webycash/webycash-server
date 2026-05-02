//! End-to-end throughput bench against a running server flavor.
//!
//! Reuses the existing docker-compose.local.yml stack instead of
//! booting a server inline (the legacy crates/server/benches/throughput.rs
//! does the inline boot for the webcash-only binary; reproducing
//! that for each new flavor would duplicate ~400 LOC). Connects via
//! plain HTTP/1.1 with parallel TCP connections — not HTTP/2
//! multiplexing — but that's enough to surface a 10x regression.
//!
//! Marked `#[ignore]` so it doesn't run under default `cargo test`.
//! Run with the compose stack up:
//!
//!   docker compose -f docker-compose.local.yml up -d
//!   cargo test --release --test throughput_bench -- --ignored --nocapture
//!
//! Reports ops/sec for each flavor's `/api/v1/health_check` endpoint
//! (read-only, doesn't mutate state). Targets the bench parity goal
//! from ROADMAP — ≥12.7k TPS Webcash, ≥5k TPS RGB/Voucher — at the
//! HTTP-frontend layer.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const PORT_WEBCASH: u16 = 8181;
const PORT_RGB_FUNGIBLE: u16 = 8182;
const PORT_VOUCHER: u16 = 8183;

const CONCURRENCY: usize = 32;
const REQUESTS_PER_THREAD: usize = 1_000;

fn server_reachable(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(500),
    )
    .is_ok()
}

fn http_post_health_check(host: &str, port: u16, body: &str) -> std::io::Result<u16> {
    let mut s = TcpStream::connect((host, port))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    let req = format!(
        "POST /api/v1/health_check HTTP/1.1\r\nHost: {host}:{port}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut buf = [0u8; 256];
    let _ = s.read(&mut buf)?;
    let head = std::str::from_utf8(&buf).unwrap_or("");
    let status: u16 = head
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(status)
}

fn bench_one(name: &str, port: u16, body: &str) {
    if !server_reachable(port) {
        eprintln!("{name} (port {port}): not reachable, skipping");
        return;
    }
    let body = Arc::new(body.to_string());
    let success = Arc::new(AtomicU64::new(0));
    let failure = Arc::new(AtomicU64::new(0));

    let t0 = Instant::now();
    let handles: Vec<_> = (0..CONCURRENCY)
        .map(|_| {
            let body = body.clone();
            let success = success.clone();
            let failure = failure.clone();
            thread::spawn(move || {
                for _ in 0..REQUESTS_PER_THREAD {
                    match http_post_health_check("127.0.0.1", port, &body) {
                        Ok(200) => {
                            success.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(_) | Err(_) => {
                            failure.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    let elapsed = t0.elapsed();

    let total = success.load(Ordering::Relaxed) + failure.load(Ordering::Relaxed);
    let ok = success.load(Ordering::Relaxed);
    let tps = ok as f64 / elapsed.as_secs_f64();
    println!(
        "{name:>16} (port {port}): {ok:>6} ok / {total} total, \
         {:>8.0} TPS, {:.2}s",
        tps,
        elapsed.as_secs_f64(),
    );
}

#[test]
#[ignore = "requires compose; run with --ignored --nocapture"]
fn throughput_health_check_per_flavor() {
    let webcash_body =
        r#"["e1.0:public:0000000000000000000000000000000000000000000000000000000000000000"]"#;
    let rgb_body = r#"["e10.0:public:0000000000000000000000000000000000000000000000000000000000000000:rgb20-bench:aabbccddeeff00112233445566778899aabbccdd"]"#;
    let voucher_body = r#"["e25.0:public:0000000000000000000000000000000000000000000000000000000000000000:credits-bench:aabbccddeeff00112233445566778899aabbccdd"]"#;

    println!();
    println!(
        "=== /api/v1/health_check throughput \
         ({CONCURRENCY} threads × {REQUESTS_PER_THREAD} req each) ==="
    );
    bench_one("webcash", PORT_WEBCASH, webcash_body);
    bench_one("rgb20", PORT_RGB_FUNGIBLE, rgb_body);
    bench_one("voucher", PORT_VOUCHER, voucher_body);
}
