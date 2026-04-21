//! Integration tests for the webycash-server REST API.
//!
//! These tests boot a real server on a random port with a Redis backend,
//! then exercise every endpoint. Requires Redis running on localhost:6379.
//!
//! Run with: cargo test --test integration
//! Skip if no Redis: set SKIP_INTEGRATION=1

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use sha2::{Digest, Sha256};

use webycash_server::api;
use webycash_server::config::{
    Config, DbBackend, DbConfig, MiningConfig, NetworkMode, ServerConfig,
};
use webycash_server::db;
use webycash_server::WebcashServer;

/// Boot a server on a random port and return (address, shared state).
/// Each call uses a fresh Redis DB and unique actor names to avoid collisions.
async fn boot_server() -> SocketAddr {
    // Select a unique Redis DB (0-15) to isolate parallel tests
    static DB_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let db_num = DB_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let redis_url = format!(
        "{}/{}",
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into()),
        db_num
    );

    let config = Config {
        compute: Default::default(),
        network: Default::default(),
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
            testnet_difficulty: 1, // trivial difficulty for fast tests
            initial_difficulty: 1,
            reports_per_epoch: 100,
            target_epoch_seconds: 1000,
            initial_mining_amount_wats: 20_000_000_000, // 200 WEB
            initial_subsidy_amount_wats: 0,
        },
    };

    let store = db::create_store(&config)
        .await
        .expect("Redis must be running");

    // Flush this DB to start fresh
    {
        let client = redis::Client::open(redis_url.as_str()).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        redis::cmd("FLUSHDB")
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
    }

    let server = WebcashServer::start(store, config.server.clone(), config.mining.clone())
        .await
        .expect("server start");

    let state = Arc::new(api::AppState {
        server,
        config: config.clone(),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
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

/// Send an HTTP request and return (status, body as Value).
async fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let uri = format!("http://{addr}{path}");
    let req = Request::builder().method(method).uri(&uri);
    let req = match body {
        Some(ref b) => req
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(b).unwrap())))
            .unwrap(),
        None => req.body(Full::new(Bytes::new())).unwrap(),
    };

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);

    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status();
    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    (status, value)
}

/// Find a preimage string whose SHA256 has at least `difficulty` leading zero bits.
fn mine_preimage(difficulty: u32, webcash_secrets: &[&str]) -> String {
    let webcash: Vec<String> = webcash_secrets.iter().map(|s| s.to_string()).collect();
    for nonce in 0u64.. {
        let preimage = serde_json::json!({
            "webcash": webcash,
            "subsidy": [],
            "timestamp": chrono::Utc::now().timestamp(),
            "difficulty": difficulty,
            "nonce": nonce,
        });
        let preimage_str = serde_json::to_string(&preimage).unwrap();
        let hash = Sha256::digest(preimage_str.as_bytes());
        let zeros = leading_zero_bits(&hash);
        if zeros >= difficulty {
            return preimage_str;
        }
    }
    unreachable!()
}

fn leading_zero_bits(hash: &[u8]) -> u32 {
    let full_zero_bytes = hash.iter().take_while(|&&b| b == 0).count() as u32;
    hash.get(full_zero_bytes as usize)
        .map_or(0, |b| b.leading_zeros())
        + full_zero_bytes * 8
}

fn random_hex64() -> String {
    use rand::Rng;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    hex::encode(bytes)
}

// ─── TESTS ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn health_endpoint() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;
    let (status, body) = request(addr, "GET", "/api/v1/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "webycash-server");
}

#[tokio::test]
async fn target_endpoint() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;
    let (status, body) = request(addr, "GET", "/api/v1/target", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["difficulty_target_bits"].is_number());
    assert!(body["epoch"].is_number());
    assert!(body["mining_amount"].is_string());
}

#[tokio::test]
async fn stats_endpoint() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;
    let (status, body) = request(addr, "GET", "/api/v1/stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["circulation"].is_string());
    assert!(body["mining_reports"].is_number());
}

#[tokio::test]
async fn not_found() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;
    let (status, body) = request(addr, "GET", "/nonexistent", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "not found");
}

#[tokio::test]
async fn cors_headers_present() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;

    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);

    let req = Request::builder()
        .method("GET")
        .uri(format!("http://{addr}/api/v1/target"))
        .body(Full::new(Bytes::new()))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let cors = resp
        .headers()
        .get("access-control-allow-origin")
        .map(|v| v.to_str().unwrap().to_string());
    assert_eq!(cors.as_deref(), Some("*"));
}

#[tokio::test]
async fn mining_report_and_replace_full_flow() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;

    // 1. Get target
    let (status, target) = request(addr, "GET", "/api/v1/target", None).await;
    assert_eq!(status, StatusCode::OK, "target response: {target}");
    let difficulty = target["difficulty_target_bits"]
        .as_u64()
        .expect(&format!("difficulty_target_bits not a number in: {target}"))
        as u32;

    // 2. Mine a token (difficulty=1 makes this instant)
    let secret_hex = random_hex64();
    let webcash_str = format!("e200.00000000:secret:{secret_hex}");
    let preimage = mine_preimage(difficulty, &[&webcash_str]);

    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/mining_report",
        Some(serde_json::json!({
            "preimage": preimage,
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mining_report failed: {body}");
    assert_eq!(body["status"], "success");

    // 3. Check the mined token exists via health_check
    let public_hash = hex::encode(Sha256::digest(secret_hex.as_bytes()));
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/health_check",
        Some(serde_json::json!([public_hash])),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "health_check failed: {body}");
    assert_eq!(body["status"], "success");
    let results = &body["results"][&public_hash];
    assert_eq!(results["spent"], false);
    assert_eq!(results["amount"], "200.00000000");

    // 4. Replace: split 200 into 150 + 50
    let new_secret1 = random_hex64();
    let new_secret2 = random_hex64();
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/replace",
        Some(serde_json::json!({
            "webcashes": [webcash_str],
            "new_webcashes": [
                format!("e150.00000000:secret:{new_secret1}"),
                format!("e50.00000000:secret:{new_secret2}"),
            ],
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replace failed: {body}");
    assert_eq!(body["status"], "success");

    // 5. Verify original is spent, new tokens exist
    let new_hash1 = hex::encode(Sha256::digest(new_secret1.as_bytes()));
    let new_hash2 = hex::encode(Sha256::digest(new_secret2.as_bytes()));
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/health_check",
        Some(serde_json::json!([public_hash, new_hash1, new_hash2])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][&public_hash]["spent"], true);
    assert_eq!(body["results"][&new_hash1]["spent"], false);
    assert_eq!(body["results"][&new_hash1]["amount"], "150.00000000");
    assert_eq!(body["results"][&new_hash2]["spent"], false);
    assert_eq!(body["results"][&new_hash2]["amount"], "50.00000000");

    // 6. Double-spend prevention: try to replace the original again
    let double_secret = random_hex64();
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/replace",
        Some(serde_json::json!({
            "webcashes": [webcash_str],
            "new_webcashes": [format!("e200.00000000:secret:{double_secret}")],
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "double-spend should fail");
    assert!(
        body["error"].as_str().unwrap().contains("spent"),
        "error should mention spent: {body}"
    );

    // 7. Stats should show mining happened
    let (status, body) = request(addr, "GET", "/api/v1/stats", None).await;
    assert_eq!(status, StatusCode::OK, "stats response: {body}");
    assert!(body["mining_reports"].as_u64().unwrap() >= 1);
    assert!(body["circulation"].as_str().is_some());
}

#[tokio::test]
async fn burn_flow() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;

    // Mine a token
    let secret_hex = random_hex64();
    let webcash_str = format!("e200.00000000:secret:{secret_hex}");
    let preimage = mine_preimage(1, &[&webcash_str]);

    let (status, _) = request(
        addr,
        "POST",
        "/api/v1/mining_report",
        Some(serde_json::json!({
            "preimage": preimage,
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Burn it
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/burn",
        Some(serde_json::json!({
            "destroy_webcash": [webcash_str],
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "burn failed: {body}");
    assert_eq!(body["status"], "success");

    // Verify spent
    let public_hash = hex::encode(Sha256::digest(secret_hex.as_bytes()));
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/health_check",
        Some(serde_json::json!([public_hash])),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][&public_hash]["spent"], true);
}

#[tokio::test]
async fn validation_errors() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;

    // Missing terms
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/replace",
        Some(serde_json::json!({
            "webcashes": ["e1.00000000:secret:aaaa"],
            "new_webcashes": ["e1.00000000:secret:bbbb"],
            "legalese": { "terms": false }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("terms"));

    // Empty webcashes
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/replace",
        Some(serde_json::json!({
            "webcashes": [],
            "new_webcashes": ["e1.00000000:secret:bbbb"],
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("empty"));

    // Empty health_check
    let (status, _body) = request(
        addr,
        "POST",
        "/api/v1/health_check",
        Some(serde_json::json!([])),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Invalid JSON body
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/api/v1/mining_report"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from("not json")))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn amount_conservation_enforced() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;

    // Mine a token
    let secret_hex = random_hex64();
    let webcash_str = format!("e200.00000000:secret:{secret_hex}");
    let preimage = mine_preimage(1, &[&webcash_str]);
    let (status, _) = request(
        addr,
        "POST",
        "/api/v1/mining_report",
        Some(serde_json::json!({
            "preimage": preimage,
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Try to create more value than input (200 -> 300)
    let steal_secret = random_hex64();
    let (status, body) = request(
        addr,
        "POST",
        "/api/v1/replace",
        Some(serde_json::json!({
            "webcashes": [webcash_str],
            "new_webcashes": [format!("e300.00000000:secret:{steal_secret}")],
            "legalese": { "terms": true }
        })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "inflation attack should fail"
    );
    assert!(
        body["error"].as_str().unwrap().contains("mismatch"),
        "should mention amount mismatch: {body}"
    );
}

#[tokio::test]
async fn streaming_mining_report() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;

    let secret_hex = random_hex64();
    let webcash_str = format!("e200.00000000:secret:{secret_hex}");
    let preimage = mine_preimage(1, &[&webcash_str]);

    // Use the /stream endpoint
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);

    let body_json = serde_json::json!({
        "preimage": preimage,
        "legalese": { "terms": true }
    });
    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/api/v1/mining_report/stream"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
            serde_json::to_vec(&body_json).unwrap(),
        )))
        .unwrap();

    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string());
    assert_eq!(content_type.as_deref(), Some("text/event-stream"));

    let body_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8_lossy(&body_bytes);

    // Should contain SSE events with accepted
    assert!(
        body_str.contains("event"),
        "SSE body should contain events: {body_str}"
    );
    assert!(
        body_str.contains("accepted"),
        "SSE body should contain accepted event: {body_str}"
    );
}

#[tokio::test]
async fn body_size_limit_enforced() {
    if std::env::var("SKIP_INTEGRATION").is_ok() {
        return;
    }
    let addr = boot_server().await;

    // Send a body larger than 1MB
    let huge_body = "x".repeat(2_000_000);
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);

    let req = Request::builder()
        .method("POST")
        .uri(format!("http://{addr}/api/v1/replace"))
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(huge_body)))
        .unwrap();

    let resp = sender.send_request(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
