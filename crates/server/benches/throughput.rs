//! Throughput benchmarks for webycash-server.
//!
//! Uses HTTP/2 multiplexing: single TCP connection, hundreds of concurrent streams.
//! No port exhaustion, measures true server throughput.
//!
//! Requires Redis on localhost:6379.
//! Run: cargo bench --bench throughput

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::TokioIo;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use webycash_server::api;
use webycash_server::config::{
    Config, DbBackend, DbConfig, MiningConfig, NetworkMode, ServerConfig,
};
use webycash_server::db;
use webycash_server::WebcashServer;

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

async fn boot_server() -> SocketAddr {
    let redis_base = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let db_num = DB_COUNTER.fetch_add(1, Ordering::Relaxed) % 16;
    let redis_url = format!("{redis_base}/{db_num}");

    let config = Config {
        server: ServerConfig {
            mode: NetworkMode::Testnet,
            bind_addr: "127.0.0.1:0".to_string(),
            db: DbConfig {
                backend: DbBackend::Redis,
                redis_url: Some(redis_url.clone()),
                dynamodb_endpoint: None,
                fdb_cluster_file: None,
            },
            cors_origin: None,
            h2: None,
        },
        mining: MiningConfig {
            testnet_difficulty: 1,
            initial_difficulty: 1,
            reports_per_epoch: 100_000,
            target_epoch_seconds: 100_000,
            initial_mining_amount_wats: 20_000_000_000,
            initial_subsidy_amount_wats: 0,
        },
    };

    {
        let client = redis::Client::open(redis_url.as_str()).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        redis::cmd("FLUSHDB")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
    }

    let store = db::create_store(&config).await.unwrap();
    let server = WebcashServer::start(store, config.server.clone(), config.mining.clone())
        .await
        .unwrap();
    let state = Arc::new(api::AppState {
        server,
        config: config.clone(),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => break,
            };
            let state = state.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let state = state.clone();
                    async move { api::router::route(state, req).await }
                });
                let _ = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(TokioIo::new(stream), service)
                .await;
            });
        }
    });

    addr
}

/// HTTP/2 multiplexed client — single TCP, unlimited concurrent streams.
struct H2Client {
    sender: hyper::client::conn::http2::SendRequest<Full<Bytes>>,
}

impl H2Client {
    async fn connect(addr: SocketAddr) -> Self {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.set_nodelay(true).unwrap();
        let (sender, conn) = hyper::client::conn::http2::handshake(
            hyper_util::rt::TokioExecutor::new(),
            TokioIo::new(stream),
        )
        .await
        .unwrap();
        tokio::spawn(conn);
        Self { sender }
    }

    async fn request(&self, method: &str, path: &str, body: Option<&[u8]>) -> u16 {
        let uri = format!("http://localhost{path}");
        let req = match body {
            Some(b) => Request::builder()
                .method(method)
                .uri(&uri)
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(b.to_vec())))
                .unwrap(),
            None => Request::builder()
                .method(method)
                .uri(&uri)
                .body(Full::new(Bytes::new()))
                .unwrap(),
        };
        // Clone sender for concurrent use (h2 sender is cheaply cloneable)
        let mut sender = self.sender.clone();
        let resp = sender.send_request(req).await.unwrap();
        let status = resp.status().as_u16();
        let _ = resp.into_body().collect().await;
        status
    }
}

fn random_hex64() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    hex::encode(bytes)
}

fn mine_preimage(difficulty: u32, webcash_str: &str) -> String {
    for nonce in 0u64.. {
        let preimage = serde_json::json!({
            "webcash": [webcash_str],
            "subsidy": [],
            "timestamp": chrono::Utc::now().timestamp(),
            "difficulty": difficulty,
            "nonce": nonce,
        });
        let s = serde_json::to_string(&preimage).unwrap();
        let hash = Sha256::digest(s.as_bytes());
        let full = hash.iter().take_while(|&&b| b == 0).count() as u32;
        let zeros = hash.get(full as usize).map_or(0, |b| b.leading_zeros()) + full * 8;
        if zeros >= difficulty {
            return s;
        }
    }
    unreachable!()
}

/// Run total_ops across concurrency concurrent H2 streams on a SINGLE connection.
async fn bench_h2<F>(
    name: &str,
    client: &H2Client,
    total_ops: usize,
    concurrency: usize,
    make_request: F,
) where
    F: Fn(usize) -> (String, String, Option<Vec<u8>>) + Send + Sync + 'static,
{
    let make_request = Arc::new(make_request);
    let sem = Arc::new(Semaphore::new(concurrency));
    let completed = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    // Warmup
    for i in 0..concurrency.min(5) {
        let (method, path, body) = (make_request)(i);
        client.request(&method, &path, body.as_deref()).await;
    }

    let start = Instant::now();
    let mut handles = Vec::with_capacity(total_ops);

    for i in 0..total_ops {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client_sender = client.sender.clone();
        let completed = completed.clone();
        let errors = errors.clone();
        let (method, path, body) = (make_request)(i);

        handles.push(tokio::spawn(async move {
            let uri = format!("http://localhost{path}");
            let req = match body {
                Some(ref b) => Request::builder()
                    .method(method.as_str())
                    .uri(&uri)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from(b.clone())))
                    .unwrap(),
                None => Request::builder()
                    .method(method.as_str())
                    .uri(&uri)
                    .body(Full::new(Bytes::new()))
                    .unwrap(),
            };
            let mut sender = client_sender;
            match sender.send_request(req).await {
                Ok(resp) => {
                    let _ = resp.into_body().collect().await;
                    completed.fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            drop(permit);
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    let elapsed = start.elapsed();
    let ok = completed.load(Ordering::Relaxed);
    let err = errors.load(Ordering::Relaxed);
    let tps = ok as f64 / elapsed.as_secs_f64();
    let avg_us = if ok > 0 {
        elapsed.as_micros() as f64 / ok as f64
    } else {
        0.0
    };
    println!(
        "  {name:<42} {ok:>6} ok {err:>3} err  {:.2}s  {tps:>8.0} TPS  {avg_us:>8.0}us/op  c={concurrency}",
        elapsed.as_secs_f64(),
    );
}

#[tokio::main]
async fn main() {
    println!("\n{}", "=".repeat(90));
    println!("  Webycash Server Throughput — HTTP/2 Multiplexed (single TCP connection)");
    println!("  Redis backend, localhost, difficulty=1");
    println!("{}\n", "=".repeat(90));

    let addr = boot_server().await;
    let client = H2Client::connect(addr).await;

    // ─── READ BENCHMARKS ────────────────────────────────────────────

    println!("--- Read operations (no DB mutation) ---");
    for c in [1, 16, 64, 256, 512] {
        bench_h2(&format!("GET /target (c={c})"), &client, 5000, c, |_| {
            ("GET".into(), "/api/v1/target".into(), None)
        })
        .await;
    }

    println!();
    for c in [1, 16, 64, 256] {
        bench_h2(&format!("GET /health (c={c})"), &client, 5000, c, |_| {
            ("GET".into(), "/api/v1/health".into(), None)
        })
        .await;
    }

    // ─── HEALTH_CHECK BENCHMARKS ────────────────────────────────────

    println!("\n--- Pre-mining 2000 tokens... ---");
    let secrets: Vec<String> = (0..2000).map(|_| random_hex64()).collect();
    let webcash_strs: Vec<String> = secrets
        .iter()
        .map(|s| format!("e200.00000000:secret:{s}"))
        .collect();

    // Mine with concurrency
    let sem = Arc::new(Semaphore::new(64));
    let mine_start = Instant::now();
    let mut mine_handles = Vec::new();
    for wc in &webcash_strs {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let preimage = mine_preimage(1, wc);
        let body = serde_json::to_vec(&serde_json::json!({
            "preimage": preimage,
            "legalese": { "terms": true }
        }))
        .unwrap();
        let mut sender = client.sender.clone();
        mine_handles.push(tokio::spawn(async move {
            let req = Request::builder()
                .method("POST")
                .uri("http://localhost/api/v1/mining_report")
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(body)))
                .unwrap();
            let resp = sender.send_request(req).await.unwrap();
            let _ = resp.into_body().collect().await;
            drop(permit);
        }));
    }
    for h in mine_handles {
        let _ = h.await;
    }
    let mine_tps = 2000.0 / mine_start.elapsed().as_secs_f64();
    println!(
        "  Pre-mined 2000 tokens in {:.2}s ({mine_tps:.0} TPS)\n",
        mine_start.elapsed().as_secs_f64()
    );

    let public_hashes: Vec<String> = secrets
        .iter()
        .map(|s| hex::encode(Sha256::digest(s.as_bytes())))
        .collect();

    println!("--- health_check (read, DB lookup) ---");
    for c in [1, 16, 64, 256] {
        let hashes = public_hashes.clone();
        bench_h2(
            &format!("health_check 1tok (c={c})"),
            &client,
            3000,
            c,
            move |i| {
                let h = &hashes[i % hashes.len()];
                let body = serde_json::to_vec(&serde_json::json!([h])).unwrap();
                ("POST".into(), "/api/v1/health_check".into(), Some(body))
            },
        )
        .await;
    }

    println!();
    for c in [1, 16, 64] {
        let hashes = public_hashes.clone();
        bench_h2(
            &format!("health_check 10tok (c={c})"),
            &client,
            2000,
            c,
            move |i| {
                let batch: Vec<&str> = (0..10)
                    .map(|j| hashes[(i * 10 + j) % hashes.len()].as_str())
                    .collect();
                let body = serde_json::to_vec(&serde_json::json!(batch)).unwrap();
                ("POST".into(), "/api/v1/health_check".into(), Some(body))
            },
        )
        .await;
    }

    // ─── REPLACE BENCHMARKS (the critical one) ──────────────────────

    println!("\n--- replace (atomic write, 1 RTT per op) ---");
    let replace_idx = Arc::new(AtomicU64::new(0));

    for c in [1, 8, 32, 64] {
        let wcs = webcash_strs.clone();
        let idx = replace_idx.clone();
        let remaining = wcs.len() as u64 - idx.load(Ordering::Relaxed);
        let ops = (remaining as usize).min(400);
        if ops < 10 {
            break;
        }

        bench_h2(
            &format!("replace 1:2 (c={c})"),
            &client,
            ops,
            c,
            move |_| {
                let i = idx.fetch_add(1, Ordering::Relaxed) as usize;
                let wc = &wcs[i % wcs.len()];
                let n1 = random_hex64();
                let n2 = random_hex64();
                let body = serde_json::to_vec(&serde_json::json!({
                    "webcashes": [wc],
                    "new_webcashes": [
                        format!("e100.00000000:secret:{n1}"),
                        format!("e50.00000000:secret:{n2}"),
                        format!("e50.00000000:secret:{}", random_hex64()),
                    ],
                    "legalese": { "terms": true }
                }))
                .unwrap();
                ("POST".into(), "/api/v1/replace".into(), Some(body))
            },
        )
        .await;
    }

    // ─── MINING REPORT BENCHMARKS ───────────────────────────────────

    println!("\n--- mining_report (PoW + write) ---");
    for c in [1, 8, 32] {
        bench_h2(&format!("mining_report (c={c})"), &client, 200, c, |_| {
            let secret = random_hex64();
            let wc = format!("e200.00000000:secret:{secret}");
            let preimage = mine_preimage(1, &wc);
            let body = serde_json::to_vec(&serde_json::json!({
                "preimage": preimage,
                "legalese": { "terms": true }
            }))
            .unwrap();
            ("POST".into(), "/api/v1/mining_report".into(), Some(body))
        })
        .await;
    }

    println!("\n{}", "=".repeat(90));
    println!("  Benchmark complete");
    println!("{}\n", "=".repeat(90));
}
