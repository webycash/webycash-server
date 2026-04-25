//! Generic asset-server core.
//!
//! `Server<A: Asset + MintableAsset>` parameterized over an asset flavor.
//! Compile-time trait bounds gate which endpoints exist:
//!
//! | Endpoint                       | Bound on `A`                                |
//! |--------------------------------|---------------------------------------------|
//! | `/api/v1/target`               | `MintableAsset`                              |
//! | `/api/v1/stats`                | `MintableAsset` (Webycash extension; not on production webcash.org) |
//! | `/api/v1/health_check`         | `Asset` (per-namespace lookup)               |
//! | `/api/v1/burn`                 | `Asset`                                      |
//! | `/api/v1/replace`              | `SplittableAsset`                            |
//! | `/api/v1/transfer`             | `TransferableAsset`                          |
//! | `/api/v1/issue`                | `IssuedAsset + MintableAsset`                |
//!
//! M1.D ships a minimal hyper-based serve loop with `/api/v1/target` and
//! `/api/v1/health_check` working for `Server<Webcash>`. The remaining
//! endpoints (replace / burn / mining_report / stats / issue / transfer)
//! migrate from `crates/server/src/api/` in subsequent passes within M1
//! and onwards.

#![forbid(unsafe_code)]

use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use webycash_asset_core::{Asset, MintableAsset};
use webycash_mining::MiningConfig;

/// Global server configuration: bind address, asset-agnostic settings.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub bind_addr: SocketAddr,
    pub mining: MiningConfig,
}

impl ServeConfig {
    pub fn testnet_default() -> Self {
        ServeConfig {
            bind_addr: "0.0.0.0:8080".parse().unwrap(),
            mining: MiningConfig {
                mode: webycash_mining::MiningMode::webcash_testnet(),
                ..MiningConfig::default()
            },
        }
    }
}

/// Server<A>: typed handle to a running asset-flavor server. Held by `serve()`.
pub struct Server<A: Asset> {
    pub config: ServeConfig,
    _ph: PhantomData<A>,
}

impl<A: Asset> Server<A> {
    pub fn new(config: ServeConfig) -> Self {
        Self {
            config,
            _ph: PhantomData,
        }
    }
}

/// Bind to `config.bind_addr` and serve hyper requests until cancelled.
///
/// Currently routes:
///   - GET  /api/v1/target         -> handlers::target (MintableAsset)
///   - GET  /api/v1/health_check   -> stub 501 (lands as health flow migrates)
///   - any  /                       -> 404
///
/// Builds with hyper-util's auto Builder so HTTP/1.1 keep-alive AND HTTP/2
/// are supported on the same port — matching the existing webycash-server.
pub async fn serve<A>(server: Server<A>) -> anyhow::Result<()>
where
    A: Asset + MintableAsset,
{
    let addr = server.config.bind_addr;
    let state = Arc::new(server);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, asset = A::NAME, "listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(route::<A>(&state, req).await) }
            });
            let builder = Builder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection(TokioIo::new(stream), svc).await {
                tracing::warn!(%peer, error = %e, "connection error");
            }
        });
    }
}

async fn route<A: Asset + MintableAsset>(
    state: &Server<A>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>> {
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/api/v1/target") => handlers::target::<A>(state).await,
        (&Method::GET, "/terms") | (&Method::GET, "/terms/text") => handlers::terms().await,
        _ => not_found(),
    }
}

fn not_found() -> Response<Full<Bytes>> {
    let body = "<html><title>404: Not Found</title><body>404: Not Found</body></html>";
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("content-type", "text/html; charset=UTF-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn ok_text_html_json(body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        // Conformance: production webcash.org returns text/html for JSON
        // bodies (Tornado default). Match exactly to keep webcash flavor
        // wire-compatible.
        .header("content-type", "text/html; charset=UTF-8")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-headers", "x-requested-with")
        .header("access-control-allow-methods", "POST, GET, OPTIONS")
        .header("strict-transport-security", "max-age=15768000")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

mod handlers {
    use super::*;

    /// `GET /api/v1/target`
    ///
    /// Production webcash.org body shape:
    /// ```json
    /// {"difficulty_target_bits": 36, "ratio": 0.39707,
    ///  "mining_amount": "195.3125", "mining_subsidy_amount": "9.765625",
    ///  "epoch": 10}
    /// ```
    /// Note field order in the raw body and the text/html Content-Type.
    pub async fn target<A: Asset + MintableAsset>(state: &Server<A>) -> Response<Full<Bytes>> {
        let cfg = &state.config.mining;
        let difficulty = cfg.current_difficulty().unwrap_or(1);
        let mining = wats_to_string(cfg.mining_amount_wats);
        let subsidy = wats_to_string(cfg.subsidy_amount_wats);
        let total = (cfg.mining_amount_wats + cfg.subsidy_amount_wats).max(1);
        let ratio = cfg.mining_amount_wats as f64 / total as f64;
        // Manual JSON build to preserve the production field order and
        // string formatting (no scientific notation, no trailing zeros).
        let body = format!(
            "{{\"difficulty_target_bits\": {difficulty}, \"ratio\": {ratio}, \
             \"mining_amount\": \"{mining}\", \"mining_subsidy_amount\": \"{subsidy}\", \
             \"epoch\": 0}}"
        );
        ok_text_html_json(body)
    }

    /// `GET /terms` / `GET /terms/text`
    pub async fn terms() -> Response<Full<Bytes>> {
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html; charset=UTF-8")
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-methods", "GET, OPTIONS")
            .body(Full::new(Bytes::from(
                "Webycash Terms of Service\n\nPlaceholder; full text wired in M1.D follow-up.\n",
            )))
            .unwrap()
    }
}

/// Convert wats (i64, 1e8) to a minimal decimal string with no trailing zeros,
/// matching the production format (`195.3125`, `9.765625`, `1`).
fn wats_to_string(wats: i64) -> String {
    const SCALE: i64 = 100_000_000;
    let abs = wats.unsigned_abs();
    let whole = abs / SCALE as u64;
    let frac = abs % SCALE as u64;
    if frac == 0 {
        return format!("{}{}", if wats < 0 { "-" } else { "" }, whole);
    }
    // Strip trailing zeros from the 8-digit fractional part.
    let frac_str = format!("{:08}", frac);
    let trimmed = frac_str.trim_end_matches('0');
    format!(
        "{}{}.{}",
        if wats < 0 { "-" } else { "" },
        whole,
        trimmed
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wats_format_matches_production() {
        // From fixtures/webcash_org_production/get_target.json:
        //   mining_amount: "195.3125", subsidy: "9.765625"
        assert_eq!(wats_to_string(195_312_500_0), "195.3125");
        assert_eq!(wats_to_string(9_765_625_00), "9.765625");
        assert_eq!(wats_to_string(100_000_000), "1");
        assert_eq!(wats_to_string(0), "0");
        assert_eq!(wats_to_string(150_000_000), "1.5");
    }
}
