//! In-Docker replace benchmark binary.
//!
//! Connects to N webycash-server instances via HTTP/2, pre-mines tokens,
//! then hammers the replace endpoint with maximum concurrency.
//!
//! Usage: bench_replace [SERVERS] [CONCURRENCY] [OPS_PER_SERVER]
//! Default: bench_replace http://server-1:8080,http://server-2:8080,http://server-3:8080 512 10000

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
        let preimage = serde_json::json!({
            "webcash": [webcash_str], "subsidy": [],
            "timestamp": chrono::Utc::now().timestamp(),
            "difficulty": 1, "nonce": nonce,
        });
        let s = serde_json::to_string(&preimage).unwrap();
        let hash = Sha256::digest(s.as_bytes());
        if hash[0] == 0 {
            return s;
        }
    }
    unreachable!()
}

/// HTTP/2 client to a single server — multiplexes thousands of streams.
struct H2Client {
    sender: hyper::client::conn::http2::SendRequest<Full<Bytes>>,
}

impl H2Client {
    async fn connect(host: &str) -> anyhow::Result<Self> {
        let uri: hyper::Uri = host.parse()?;
        let addr = format!(
            "{}:{}",
            uri.host().unwrap_or("127.0.0.1"),
            uri.port_u16().unwrap_or(8080)
        );
        let stream = tokio::net::TcpStream::connect(&addr).await?;
        stream.set_nodelay(true)?;
        let (sender, conn) = hyper::client::conn::http2::handshake(
            hyper_util::rt::TokioExecutor::new(),
            TokioIo::new(stream),
        )
        .await?;
        tokio::spawn(conn);
        Ok(Self { sender })
    }

    async fn post(&self, path: &str, body: Vec<u8>) -> u16 {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
            .unwrap();
        let mut sender = self.sender.clone();
        match sender.send_request(req).await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let _ = resp.into_body().collect().await;
                status
            }
            Err(_) => 0,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let servers_str = args
        .get(1)
        .cloned()
        .or_else(|| std::env::var("SERVERS").ok())
        .unwrap_or_else(|| {
            "http://server-1:8080,http://server-2:8080,http://server-3:8080".to_string()
        });
    let servers: Vec<&str> = servers_str.split(',').collect();
    let concurrency: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(512);
    let ops_per_server: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(10000);

    println!("{}", "=".repeat(70));
    println!("  Rust Replace Benchmark (HTTP/2 multiplexed, inside Docker)");
    println!("  Servers: {}", servers.join(", "));
    println!("  Concurrency: {concurrency}, Ops/server: {ops_per_server}");
    println!("{}", "=".repeat(70));

    // Connect H2 clients
    let mut clients = Vec::new();
    for server in &servers {
        print!("  Connecting to {server}...");
        loop {
            match H2Client::connect(server).await {
                Ok(c) => {
                    // Health check
                    let status = c.post("/api/v1/health", Vec::new()).await;
                    if status == 200 {
                        println!(" UP");
                        clients.push(Arc::new(c));
                        break;
                    }
                }
                Err(_) => {}
            }
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }

    // Pre-mine tokens
    println!("\n--- Pre-mining {ops_per_server} tokens per server ---");
    let mine_start = Instant::now();
    let sem = Arc::new(Semaphore::new(128));
    let all_tokens: Vec<Vec<(String, String)>> =
        futures::future::join_all(clients.iter().enumerate().map(|(idx, client)| {
            let client = client.clone();
            let sem = sem.clone();
            async move {
                let mut tokens = Vec::with_capacity(ops_per_server);
                let mut handles = Vec::new();
                for _ in 0..ops_per_server {
                    let permit = sem.clone().acquire_owned().await.unwrap();
                    let client = client.clone();
                    handles.push(tokio::spawn(async move {
                        let secret = random_hex64();
                        let wc = format!("e200.00000000:secret:{secret}");
                        let preimage = mine_preimage(&wc);
                        let body = serde_json::to_vec(&serde_json::json!({
                            "preimage": preimage,
                            "legalese": {"terms": true}
                        }))
                        .unwrap();
                        let status = client.post("/api/v1/mining_report", body).await;
                        drop(permit);
                        if status == 200 {
                            Some((secret, wc))
                        } else {
                            None
                        }
                    }));
                }
                for h in handles {
                    if let Ok(Some(t)) = h.await {
                        tokens.push(t);
                    }
                }
                println!("  Server {}: {} tokens mined", idx + 1, tokens.len());
                tokens
            }
        }))
        .await;
    let total_mined: usize = all_tokens.iter().map(|t| t.len()).sum();
    let mine_tps = total_mined as f64 / mine_start.elapsed().as_secs_f64();
    println!(
        "  Total: {total_mined} tokens in {:.1}s ({mine_tps:.0} mine/s)\n",
        mine_start.elapsed().as_secs_f64()
    );

    // Benchmark replace
    println!("--- Replace benchmark ---");

    let total_ok = Arc::new(AtomicU64::new(0));
    let total_err = Arc::new(AtomicU64::new(0));
    let sem = Arc::new(Semaphore::new(concurrency));

    // Build all replace args
    let mut all_args: Vec<(Arc<H2Client>, String)> = Vec::new();
    for (tokens, client) in all_tokens.iter().zip(clients.iter()) {
        for (_, wc) in tokens {
            let n1 = random_hex64();
            let n2 = random_hex64();
            let body = serde_json::to_vec(&serde_json::json!({
                "webcashes": [wc],
                "new_webcashes": [
                    format!("e100.00000000:secret:{n1}"),
                    format!("e100.00000000:secret:{n2}"),
                ],
                "legalese": {"terms": true}
            }))
            .unwrap();
            all_args.push((client.clone(), String::from_utf8(body).unwrap()));
        }
    }

    let total_ops = all_args.len();
    println!("  Total replace operations: {total_ops}");

    let start = Instant::now();
    let mut handles = Vec::with_capacity(total_ops);

    for (client, body) in all_args {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let ok = total_ok.clone();
        let err = total_err.clone();
        handles.push(tokio::spawn(async move {
            let status = client.post("/api/v1/replace", body.into_bytes()).await;
            if status == 200 {
                ok.fetch_add(1, Ordering::Relaxed);
            } else {
                err.fetch_add(1, Ordering::Relaxed);
            }
            drop(permit);
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();
    let ok = total_ok.load(Ordering::Relaxed);
    let err = total_err.load(Ordering::Relaxed);
    let tps = ok as f64 / elapsed.as_secs_f64();

    println!("\n  {total_ops} ops in {:.2}s", elapsed.as_secs_f64());
    println!("  {ok} ok, {err} err");
    println!(
        "  {tps:.0} TPS ({} servers × c={concurrency})",
        servers.len()
    );
    println!("  {:.0} TPS per server", tps / servers.len() as f64);
    println!(
        "  {:.3}ms avg latency",
        elapsed.as_millis() as f64 / ok as f64
    );

    println!("\n{}", "=".repeat(70));
    Ok(())
}
