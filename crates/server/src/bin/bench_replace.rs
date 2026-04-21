//! In-Docker replace benchmark. Persistent HTTP/1.1 keep-alive connection pool.
//!
//! Each server gets N persistent connections. Requests dispatched via mpsc channels.
//! Zero TCP handshake per request — connections auto-reconnect on failure.
//!
//! Usage: bench_replace [SERVERS] [CONCURRENCY] [OPS_PER_SERVER] [CONNS_PER_SERVER]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, Semaphore};

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

struct Job {
    method: &'static str,
    path: String,
    body: Vec<u8>,
    reply: tokio::sync::oneshot::Sender<u16>,
}

/// Pool of persistent HTTP/1.1 keep-alive connections to one server.
struct ConnPool {
    txs: Vec<mpsc::Sender<Job>>,
    idx: AtomicU64,
}

impl ConnPool {
    async fn new(addr: &str, n: usize) -> Self {
        let txs: Vec<_> = (0..n)
            .map(|_| {
                let (tx, rx) = mpsc::channel::<Job>(2048);
                let addr = addr.to_string();
                tokio::spawn(Self::worker(addr, rx));
                tx
            })
            .collect();
        Self {
            txs,
            idx: AtomicU64::new(0),
        }
    }

    async fn worker(addr: String, mut rx: mpsc::Receiver<Job>) {
        loop {
            let stream = match tokio::net::TcpStream::connect(&addr).await {
                Ok(s) => {
                    let _ = s.set_nodelay(true);
                    s
                }
                Err(_) => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let (mut sender, conn) =
                match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
                    Ok(r) => r,
                    Err(_) => continue,
                };
            tokio::spawn(conn);

            while let Some(job) = rx.recv().await {
                if !sender.is_ready() {
                    let _ = job.reply.send(0);
                    break;
                }
                let req = Request::builder()
                    .method(job.method)
                    .uri(format!("http://{}{}", addr, job.path))
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(job.body)))
                    .unwrap();
                match sender.send_request(req).await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let _ = resp.into_body().collect().await;
                        let _ = job.reply.send(status);
                    }
                    Err(_) => {
                        let _ = job.reply.send(0);
                        break;
                    }
                }
            }
        }
    }

    async fn request(&self, method: &'static str, path: &str, body: Vec<u8>) -> u16 {
        let i = self.idx.fetch_add(1, Ordering::Relaxed) as usize % self.txs.len();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = self.txs[i]
            .send(Job {
                method,
                path: path.to_string(),
                body,
                reply: tx,
            })
            .await;
        rx.await.unwrap_or(0)
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
    let conns_per_server: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(64);

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

    eprintln!("{}", "=".repeat(70));
    eprintln!("  Rust Replace Bench (keep-alive pool, inside Docker)");
    eprintln!("  Servers: {:?}", addrs);
    eprintln!("  c={concurrency} ops/srv={ops_per_server} conns/srv={conns_per_server}");
    eprintln!("{}", "=".repeat(70));

    // Create pools + wait for health
    let pools: Vec<Arc<ConnPool>> = futures::future::join_all(addrs.iter().map(|addr| {
        let addr = addr.clone();
        async move {
            let pool = Arc::new(ConnPool::new(&addr, conns_per_server).await);
            loop {
                if pool.request("GET", "/api/v1/health", Vec::new()).await == 200 {
                    eprintln!("  {addr} UP");
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
            pool
        }
    }))
    .await;

    // Pre-mine
    eprintln!("\n--- Mining {ops_per_server}/server ---");
    let t0 = Instant::now();
    let sem = Arc::new(Semaphore::new(256));
    let all_tokens: Vec<Vec<String>> =
        futures::future::join_all(pools.iter().enumerate().map(|(i, pool)| {
            let pool = pool.clone();
            let sem = sem.clone();
            async move {
                let handles: Vec<_> = (0..ops_per_server)
                    .map(|_| {
                        let pool = pool.clone();
                        let sem = sem.clone();
                        tokio::spawn(async move {
                            let _p = sem.acquire_owned().await.unwrap();
                            let secret = random_hex64();
                            let wc = format!("e200.00000000:secret:{secret}");
                            let preimage = mine_preimage(&wc);
                            let body = serde_json::to_vec(&serde_json::json!({
                                "preimage": preimage, "legalese": {"terms": true}
                            }))
                            .unwrap();
                            if pool.post("/api/v1/mining_report", body).await == 200 {
                                Some(wc)
                            } else {
                                None
                            }
                        })
                    })
                    .collect();
                let tokens: Vec<String> = futures::future::join_all(handles)
                    .await
                    .into_iter()
                    .filter_map(|r| r.ok().flatten())
                    .collect();
                eprintln!("  srv{}: {} tokens", i + 1, tokens.len());
                tokens
            }
        }))
        .await;
    let mined = all_tokens.iter().map(|t| t.len()).sum::<usize>();
    eprintln!(
        "  {mined} in {:.1}s ({:.0}/s)\n",
        t0.elapsed().as_secs_f64(),
        mined as f64 / t0.elapsed().as_secs_f64()
    );

    // Build workload
    let work: Vec<(usize, Vec<u8>)> = all_tokens
        .iter()
        .enumerate()
        .flat_map(|(idx, tokens)| {
            tokens.iter().map(move |wc| {
                let body = serde_json::to_vec(&serde_json::json!({
                    "webcashes": [wc],
                    "new_webcashes": [
                        format!("e100.00000000:secret:{}", random_hex64()),
                        format!("e100.00000000:secret:{}", random_hex64()),
                    ],
                    "legalese": {"terms": true}
                }))
                .unwrap();
                (idx, body)
            })
        })
        .collect();

    let n = work.len();
    eprintln!("--- REPLACE: {n} ops, c={concurrency}, {conns_per_server} conns/srv ---");

    let ok = Arc::new(AtomicU64::new(0));
    let err = Arc::new(AtomicU64::new(0));
    let sem = Arc::new(Semaphore::new(concurrency));

    let start = Instant::now();
    let handles: Vec<_> = work
        .into_iter()
        .map(|(pi, body)| {
            let pool = pools[pi].clone();
            let sem = sem.clone();
            let ok = ok.clone();
            let err = err.clone();
            tokio::spawn(async move {
                let _p = sem.acquire_owned().await.unwrap();
                if pool.request("POST", "/api/v1/replace", body).await == 200 {
                    ok.fetch_add(1, Ordering::Relaxed);
                } else {
                    err.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    futures::future::join_all(handles).await;

    let elapsed = start.elapsed();
    let ok_n = ok.load(Ordering::Relaxed);
    let err_n = err.load(Ordering::Relaxed);
    let tps = ok_n as f64 / elapsed.as_secs_f64();

    eprintln!("\n  {n} ops  {:.2}s", elapsed.as_secs_f64());
    eprintln!("  {ok_n} ok  {err_n} err");
    eprintln!("  {tps:.0} TPS total");
    eprintln!("  {:.0} TPS/server", tps / addrs.len() as f64);
    if ok_n > 0 {
        eprintln!(
            "  {:.3}ms latency",
            elapsed.as_millis() as f64 / ok_n as f64
        );
    }
    eprintln!("{}", "=".repeat(70));
    println!("{:.0}", tps);
    Ok(())
}

impl ConnPool {
    async fn post(&self, path: &str, body: Vec<u8>) -> u16 {
        self.request("POST", path, body).await
    }
}
