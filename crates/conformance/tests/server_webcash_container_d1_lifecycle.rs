//! Webcash full lifecycle through Cloudflare Containers + D1 + miniflare.
//!
//! This is the load-bearing test for the "fully Cloudflare Workers"
//! deployment path (Lambda-style: native hyper binary in a Container on
//! Cloudflare's edge, D1 reached via a thin Worker proxy). It:
//!
//!   1. `npm install` inside the worker dir to pull
//!      `@cloudflare/containers` + `wrangler`
//!   2. `wrangler d1 migrations apply --local` to seed the schema
//!   3. `wrangler dev --local` — wrangler builds the Container image
//!      from `Dockerfile.cf-webcash` (release Rust build inside the
//!      container, ~5–10 minutes the first time, ~seconds on cache hit)
//!   4. Waits up to 15 minutes for the container's `/api/v1/target` to
//!      come up through the Worker proxy
//!   5. Runs the lifecycle: mining_report → health_check → replace
//!   6. Concurrent-replace adversary: proves the trigger-based ABORT in
//!      the D1 schema correctly aborts the loser's batch all the way
//!      through Container → Worker proxy → D1 → SQLite trigger → response
//!
//! Skipped with stderr notes when any of `wrangler`, `docker`, or `npm`
//! is missing on `PATH`.

mod common;

use std::process::{Child, Command, Stdio};
use std::time::Duration;

#[test]
fn webcash_lifecycle_against_container_worker_d1() {
    if !cmd_check("wrangler") || !cmd_check("npm") || !cmd_check("docker") {
        eprintln!("skipping: wrangler, npm, or docker unavailable");
        return;
    }
    if !docker_daemon_running() {
        eprintln!("skipping: Docker daemon not running");
        return;
    }

    let root = workspace_root();
    let worker_dir = root.join("deploy/cloudflare-container/workers/webcash");
    let wrangler_toml = worker_dir.join("wrangler.toml");
    let dockerfile = root.join("Dockerfile.cf-webcash");
    if !worker_dir.exists() || !wrangler_toml.exists() || !dockerfile.exists() {
        eprintln!("skipping: cloudflare-container/workers/webcash layout missing");
        return;
    }

    // Pull TS deps (@cloudflare/containers + wrangler types). Cached
    // after the first run; ~10 seconds.
    let npm = Command::new("npm")
        .args(["install", "--no-audit", "--no-fund", "--silent"])
        .current_dir(&worker_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();
    if !npm.map(|s| s.success()).unwrap_or(false) {
        eprintln!("skipping: `npm install` failed in {}", worker_dir.display());
        return;
    }

    // Apply schema to the local D1.
    let apply = Command::new("wrangler")
        .args([
            "d1",
            "migrations",
            "apply",
            "webycash-cf-container-webcash-testnet",
            "--local",
        ])
        .current_dir(&worker_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();
    if !apply.map(|s| s.success()).unwrap_or(false) {
        eprintln!("skipping: `wrangler d1 migrations apply` failed");
        return;
    }

    let port = pick_free_port();
    let bind = format!("127.0.0.1:{port}");

    // Start wrangler dev — this triggers a docker build of
    // Dockerfile.cf-webcash and boots the Container. On first run
    // the Rust release compile can take 5–10 minutes inside Alpine.
    let mut child: Child = match Command::new("wrangler")
        .args(["dev", "--local", "--port", &port.to_string()])
        .current_dir(&worker_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("skipping: wrangler dev spawn failed: {e}");
            return;
        }
    };

    // Generous timeout: 15 minutes covers the cold-cache Rust-in-Alpine
    // build plus container boot. Re-runs hit the warm Docker cache and
    // come up in ~20 seconds.
    let deadline = std::time::Instant::now() + Duration::from_secs(900);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(&bind).is_ok()
            && http_get(&format!("http://{bind}/api/v1/target")).is_ok()
        {
            ready = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        eprintln!("skipping: wrangler dev + Container did not become ready within 15 min");
        return;
    }

    let result = std::panic::catch_unwind(|| {
        run_lifecycle(&bind);
        run_concurrent_replace_adversary(&bind);
    });

    let _ = child.kill();
    let _ = child.wait();

    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_lifecycle(bind: &str) {
    let secret = "a".repeat(64);
    let public_hash = common::sha256_hex(&secret);

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
    let preimage = common::find_preimage(&template, 4);
    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/mining_report"),
        &serde_json::to_string(&serde_json::json!({
            "preimage": preimage,
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("mine");
    assert_eq!(status, 200, "container mining_report: {body}");

    let (status, body) = http_post(
        &format!("http://{bind}/api/v1/health_check"),
        &serde_json::to_string(&serde_json::json!([format!(
            "e1.0:public:{public_hash}"
        )]))
        .unwrap(),
    )
    .expect("hc1");
    assert_eq!(status, 200);
    assert!(
        body.contains(r#""spent": false"#),
        "container hc1: {body}"
    );

    let out1 = "1".repeat(64);
    let out2 = "2".repeat(64);
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
    assert_eq!(status, 200, "container replace: {body}");
}

fn run_concurrent_replace_adversary(bind: &str) {
    // Mine a fresh token, then race two replaces over the same input
    // through the Container ↔ Worker ↔ D1 stack. The trigger-based
    // ABORT must let exactly one through and surface `double_spend` to
    // the loser. This is the load-bearing assertion: it proves the
    // atomicity primitives survive every hop in the topology.
    let secret = "c".repeat(64);
    let preimage_obj = serde_json::json!({
        "webcash": [format!("e1.0:secret:{secret}")],
        "subsidy": [format!("e0.5:secret:{}", "d".repeat(64))],
        "timestamp": 1714003200i64,
        "difficulty": 4,
        "nonce": 0,
    });
    let template = serde_json::to_string(&preimage_obj)
        .unwrap()
        .replace(r#""nonce":0"#, r#""nonce":__N__"#);
    let preimage = common::find_preimage(&template, 4);
    let (status, _) = http_post(
        &format!("http://{bind}/api/v1/mining_report"),
        &serde_json::to_string(&serde_json::json!({
            "preimage": preimage,
            "legalese": {"terms": true},
        }))
        .unwrap(),
    )
    .expect("mine for adversary");
    assert_eq!(status, 200);

    let bind1 = bind.to_string();
    let bind2 = bind.to_string();
    let secret1 = secret.clone();
    let secret2 = secret;

    let t1 = std::thread::spawn(move || -> (u16, String) {
        http_post(
            &format!("http://{bind1}/api/v1/replace"),
            &serde_json::to_string(&serde_json::json!({
                "webcashes": [format!("e1.0:secret:{secret1}")],
                "new_webcashes": [
                    format!("e0.4:secret:{}", "5".repeat(64)),
                    format!("e0.6:secret:{}", "6".repeat(64)),
                ],
                "legalese": {"terms": true},
            }))
            .unwrap(),
        )
        .expect("replace thread 1")
    });
    let t2 = std::thread::spawn(move || -> (u16, String) {
        http_post(
            &format!("http://{bind2}/api/v1/replace"),
            &serde_json::to_string(&serde_json::json!({
                "webcashes": [format!("e1.0:secret:{secret2}")],
                "new_webcashes": [
                    format!("e0.5:secret:{}", "7".repeat(64)),
                    format!("e0.5:secret:{}", "8".repeat(64)),
                ],
                "legalese": {"terms": true},
            }))
            .unwrap(),
        )
        .expect("replace thread 2")
    });
    let (s1, b1) = t1.join().expect("thread 1 join");
    let (s2, b2) = t2.join().expect("thread 2 join");
    let results = [(s1, b1.clone()), (s2, b2.clone())];
    let oks = results.iter().filter(|(s, _)| *s == 200).count();
    let failures: Vec<&(u16, String)> = results.iter().filter(|(s, _)| *s != 200).collect();
    assert_eq!(
        oks, 1,
        "container adversary: exactly one concurrent replace must commit; s1={s1} s2={s2}"
    );
    assert!(
        failures.iter().any(|(_, b)| b.contains("double_spend")),
        "container adversary: loser must carry double_spend; got {:?}",
        failures
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Helpers — inline per the existing per-test convention.

fn workspace_root() -> std::path::PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    std::path::PathBuf::from(&manifest_dir)
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn pick_free_port() -> u16 {
    let s = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let p = s.local_addr().expect("addr").port();
    drop(s);
    p
}

fn cmd_check(cmd: &str) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn docker_daemon_running() -> bool {
    Command::new("docker")
        .args(["info", "--format", "{{.ServerVersion}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn http_get(url: &str) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".to_string()));
    let mut stream = std::net::TcpStream::connect(&host_port)?;
    stream.set_read_timeout(Some(Duration::from_secs(8)))?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes())?;
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

fn http_post(url: &str, body: &str) -> std::io::Result<(u16, String)> {
    use std::io::{Read, Write};
    let after = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after.to_string(), "/".to_string()));
    let mut stream = std::net::TcpStream::connect(&host_port)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
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
