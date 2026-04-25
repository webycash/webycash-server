//! Boots the actual `webycash-server-webcash` binary, hits `/api/v1/target`,
//! and asserts byte-shape compatibility with the captured production fixture.

use std::process::{Command, Stdio};
use std::time::Duration;

const BIN_NAME: &str = "webycash-server-webcash";

fn binary_path() -> std::path::PathBuf {
    // Tests run from the conformance crate dir; the workspace target/ is two up.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let workspace_target = std::path::PathBuf::from(&manifest_dir)
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("debug")
        .join(BIN_NAME);
    if workspace_target.exists() {
        return workspace_target;
    }
    // Fallback: rely on PATH lookup.
    std::path::PathBuf::from(BIN_NAME)
}

fn pick_port() -> u16 {
    // Random ephemeral port. Bind a TcpListener to discover one, then close it.
    let socket = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let port = socket.local_addr().expect("addr").port();
    drop(socket);
    port
}

#[test]
fn target_endpoint_matches_production_shape() {
    let bin = binary_path();
    if !bin.exists() {
        eprintln!(
            "skipping: {} not built. Run `cargo build -p webycash-server-webcash` first.",
            bin.display()
        );
        return;
    }
    let port = pick_port();
    let bind = format!("127.0.0.1:{port}");

    let mut child = Command::new(&bin)
        .env("WEBCASH_BIND_ADDR", &bind)
        .env("WEBCASH_MODE", "testnet")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server-webcash");

    // Wait for the bind. Up to 5s; poll TCP connect.
    let url = format!("http://{bind}/api/v1/target");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect(&bind).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Curl-like fetch using std::net (no extra deps in this crate).
    let response_body = fetch_get(&url).expect("fetch /api/v1/target");

    // Body must be valid JSON parsed by serde_json (production also serves
    // valid JSON despite the text/html Content-Type).
    let parsed: serde_json::Value =
        serde_json::from_str(&response_body).expect("body is JSON");

    let obj = parsed.as_object().expect("object");
    assert!(obj.contains_key("difficulty_target_bits"));
    assert!(obj.contains_key("ratio"));
    assert!(obj.contains_key("mining_amount"));
    assert!(obj.contains_key("mining_subsidy_amount"));
    assert!(obj.contains_key("epoch"));

    // Values follow production conventions:
    assert!(obj["difficulty_target_bits"].as_u64().is_some());
    assert!(obj["epoch"].as_u64().is_some());
    assert!(obj["mining_amount"].is_string());
    assert!(obj["mining_subsidy_amount"].is_string());
    assert!(obj["ratio"].is_number());

    // Field order in the raw body matches production:
    let target_idx = response_body.find("difficulty_target_bits").unwrap();
    let ratio_idx = response_body.find("ratio").unwrap();
    let mining_idx = response_body.find("mining_amount").unwrap();
    let subsidy_idx = response_body.find("mining_subsidy_amount").unwrap();
    let epoch_idx = response_body.find("epoch").unwrap();
    assert!(target_idx < ratio_idx, "target must precede ratio");
    assert!(ratio_idx < mining_idx, "ratio must precede mining_amount");
    assert!(
        mining_idx < subsidy_idx,
        "mining_amount must precede mining_subsidy_amount"
    );
    assert!(subsidy_idx < epoch_idx, "subsidy must precede epoch");

    let _ = child.kill();
    let _ = child.wait();
}

/// Minimal HTTP/1.1 GET. Avoids pulling reqwest/hyper into this test crate.
fn fetch_get(url: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};
    // Naive parsing of `http://host:port/path`.
    let after_scheme = url.strip_prefix("http://").unwrap_or(url);
    let (host_port, path) = after_scheme
        .split_once('/')
        .map(|(h, p)| (h.to_string(), format!("/{p}")))
        .unwrap_or((after_scheme.to_string(), "/".to_string()));

    let mut stream = std::net::TcpStream::connect(&host_port)?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    Ok(text[body_start..].to_string())
}
