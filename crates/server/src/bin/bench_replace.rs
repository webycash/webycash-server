//! In-Docker replace benchmark. HTTP/1.1 connection pool per server.
//!
//! Usage: bench_replace [SERVERS] [CONCURRENCY] [OPS_PER_SERVER]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

fn random_hex64() -> String {
    use rand::Rng;
    hex::encode(rand::thread_rng().gen::<[u8; 32]>())
}

fn mine_preimage(webcash_str: &str) -> String {
    for nonce in 0u64.. {
        let s = serde_json::to_string(&serde_json::json!({
            "webcash": [webcash_str], "subsidy": [],
            "timestamp": chrono::Utc::now().timestamp(),
            "difficulty": 1, "nonce": nonce,
        }))
        .unwrap();
        if Sha256::digest(s.as_bytes())[0] == 0 {
            return s;
        }
    }
    unreachable!()
}

async fn http_get(addr: &str, path: &str) -> u16 {
    let stream = match tokio::net::TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let _ = stream.set_nodelay(true);
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(r) => r,
        Err(_) => return 0,
    };
    tokio::spawn(conn);
    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{addr}{path}"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    match sender.send_request(req).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let _ = resp.into_body().collect().await;
            status
        }
        Err(_) => 0,
    }
}

async fn http_post(addr: &str, path: &str, body: &[u8]) -> u16 {
    let stream = match tokio::net::TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let _ = stream.set_nodelay(true);
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await
    {
        Ok(r) => r,
        Err(_) => return 0,
    };
    tokio::spawn(conn);

    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}{path}"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_vec())))
        .unwrap();

    match sender.send_request(req).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let _ = resp.into_body().collect().await;
            status
        }
        Err(_) => 0,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let servers_str = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("SERVERS").ok())
        .unwrap_or_else(|| "http://server-1:8080,http://server-2:8080,http://server-3:8080".into());
    let concurrency: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(256);
    let ops_per_server: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5000);

    // Parse server addresses (host:port)
    let addrs: Vec<String> = servers_str
        .split(',')
        .map(|s| {
            let uri: hyper::Uri = s.parse().unwrap();
            format!(
                "{}:{}",
                uri.host().unwrap_or("127.0.0.1"),
                uri.port_u16().unwrap_or(8080)
            )
        })
        .collect();

    eprintln!("======================================================================");
    eprintln!("  Rust Replace Benchmark (HTTP/1.1 pool, inside Docker)");
    eprintln!("  Servers: {:?}", addrs);
    eprintln!("  Concurrency: {concurrency}, Ops/server: {ops_per_server}");
    eprintln!("======================================================================");

    // Wait for servers
    for addr in &addrs {
        eprint!("  Connecting to {addr}...");
        loop {
            if http_get(addr, "/api/v1/health").await == 200 {
                eprintln!(" UP");
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    // Pre-mine tokens concurrently
    eprintln!("\n--- Pre-mining {ops_per_server} tokens per server ---");
    let mine_start = Instant::now();
    let sem = Arc::new(Semaphore::new(128));

    let all_tokens: Vec<Vec<String>> =
        futures::future::join_all(addrs.iter().enumerate().map(|(idx, addr)| {
            let addr = addr.clone();
            let sem = sem.clone();
            async move {
                let mut handles = Vec::new();
                for _ in 0..ops_per_server {
                    let permit = sem.clone().acquire_owned().await.unwrap();
                    let addr = addr.clone();
                    handles.push(tokio::spawn(async move {
                        let secret = random_hex64();
                        let wc = format!("e200.00000000:secret:{secret}");
                        let preimage = mine_preimage(&wc);
                        let body = serde_json::to_vec(&serde_json::json!({
                            "preimage": preimage,
                            "legalese": {"terms": true}
                        }))
                        .unwrap();
                        let status = http_post(&addr, "/api/v1/mining_report", &body).await;
                        drop(permit);
                        if status == 200 {
                            Some(wc)
                        } else {
                            None
                        }
                    }));
                }
                let tokens: Vec<String> = futures::future::join_all(handles)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok().flatten())
                    .collect();
                eprintln!("  Server {}: {} tokens", idx + 1, tokens.len());
                tokens
            }
        }))
        .await;

    let total_mined: usize = all_tokens.iter().map(|t| t.len()).sum();
    let mine_elapsed = mine_start.elapsed();
    eprintln!(
        "  Total: {} tokens in {:.1}s ({:.0} mine/s)\n",
        total_mined,
        mine_elapsed.as_secs_f64(),
        total_mined as f64 / mine_elapsed.as_secs_f64()
    );

    // Build replace workload
    let mut work: Vec<(String, Vec<u8>)> = Vec::new();
    for (tokens, addr) in all_tokens.iter().zip(addrs.iter()) {
        for wc in tokens {
            let body = serde_json::to_vec(&serde_json::json!({
                "webcashes": [wc],
                "new_webcashes": [
                    format!("e100.00000000:secret:{}", random_hex64()),
                    format!("e100.00000000:secret:{}", random_hex64()),
                ],
                "legalese": {"terms": true}
            }))
            .unwrap();
            work.push((addr.clone(), body));
        }
    }

    let total_ops = work.len();
    eprintln!("--- Replace benchmark: {total_ops} ops, c={concurrency} ---");

    let ok_count = Arc::new(AtomicU64::new(0));
    let err_count = Arc::new(AtomicU64::new(0));
    let sem = Arc::new(Semaphore::new(concurrency));

    let start = Instant::now();
    let handles: Vec<_> = work
        .into_iter()
        .map(|(addr, body)| {
            let sem = sem.clone();
            let ok = ok_count.clone();
            let err = err_count.clone();
            tokio::spawn(async move {
                let permit = sem.acquire_owned().await.unwrap();
                let status = http_post(&addr, "/api/v1/replace", &body).await;
                if status == 200 {
                    ok.fetch_add(1, Ordering::Relaxed);
                } else {
                    err.fetch_add(1, Ordering::Relaxed);
                }
                drop(permit);
            })
        })
        .collect();

    futures::future::join_all(handles).await;
    let elapsed = start.elapsed();

    let ok = ok_count.load(Ordering::Relaxed);
    let err = err_count.load(Ordering::Relaxed);
    let tps = ok as f64 / elapsed.as_secs_f64();

    eprintln!();
    eprintln!("  {} ops in {:.2}s", total_ops, elapsed.as_secs_f64());
    eprintln!("  {} ok, {} err", ok, err);
    eprintln!(
        "  {:.0} TPS ({} servers × c={})",
        tps,
        addrs.len(),
        concurrency
    );
    eprintln!("  {:.0} TPS per server", tps / addrs.len() as f64);
    if ok > 0 {
        eprintln!(
            "  {:.3}ms avg latency",
            elapsed.as_millis() as f64 / ok as f64
        );
    }
    eprintln!("\n======================================================================");

    // Print machine-readable result for CI
    println!("{:.0}", tps);

    Ok(())
}
