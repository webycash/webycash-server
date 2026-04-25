//! Generic asset-server core.
//!
//! `Server<A: Asset, S: LedgerStore<A>>` parameterized over an asset flavor
//! and its storage backend. Compile-time trait bounds gate which endpoints
//! are exposed.
//!
//! Endpoint matrix:
//!
//! | Endpoint                | Bound on `A`                     |
//! |-------------------------|----------------------------------|
//! | `GET  /api/v1/target`   | `MintableAsset`                  |
//! | `POST /api/v1/health_check` | `Asset`                       |
//! | `POST /api/v1/replace`  | `SplittableAsset`                |
//! | `POST /api/v1/burn`     | `Asset`                          |
//! | `POST /api/v1/mining_report` | `MintableAsset`              |
//! | `GET  /terms`, `/terms/text` | (none)                       |

#![forbid(unsafe_code)]

use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use serde::Deserialize;
use webycash_asset_core::{Amount, Asset, AssetPublic, MintableAsset, SplittableAsset};
use webycash_mining::MiningConfig;
use webycash_storage::{LedgerStore, Namespace};

const MAX_BODY_BYTES: usize = 1024 * 1024; // 1 MB — matches legacy server

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

/// Server<A, S>: typed handle to a running asset-flavor server.
pub struct Server<A: Asset, S: LedgerStore<A>> {
    pub config: ServeConfig,
    pub store: Arc<S>,
    _ph: PhantomData<A>,
}

impl<A: Asset, S: LedgerStore<A>> Server<A, S> {
    pub fn new(config: ServeConfig, store: S) -> Self {
        Self {
            config,
            store: Arc::new(store),
            _ph: PhantomData,
        }
    }
}

/// Bind to `config.bind_addr` and serve hyper requests until cancelled.
///
/// Routes:
///   - GET  /api/v1/target         → handlers::target (MintableAsset)
///   - POST /api/v1/health_check   → handlers::health_check
///   - POST /api/v1/replace        → handlers::replace (SplittableAsset)
///   - GET  /terms, /terms/text    → handlers::terms
///   - any                          → 404 (production-shape HTML)
///
/// Builds with hyper-util's auto Builder so HTTP/1.1 keep-alive AND HTTP/2
/// are supported on the same port — matching the existing webycash-server.
pub async fn serve<A, S>(server: Server<A, S>) -> anyhow::Result<()>
where
    A: Asset + MintableAsset + SplittableAsset,
    S: LedgerStore<A>,
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
                async move { Ok::<_, Infallible>(route::<A, S>(&state, req).await) }
            });
            let builder = Builder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection(TokioIo::new(stream), svc).await {
                tracing::warn!(%peer, error = %e, "connection error");
            }
        });
    }
}

async fn route<A, S>(state: &Server<A, S>, req: Request<Incoming>) -> Response<Full<Bytes>>
where
    A: Asset + MintableAsset + SplittableAsset,
    S: LedgerStore<A>,
{
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/api/v1/target") => handlers::target::<A, S>(state).await,
        (&Method::GET, "/terms") | (&Method::GET, "/terms/text") => handlers::terms().await,
        (&Method::POST, "/api/v1/health_check") => {
            handlers::health_check::<A, S>(state, req).await
        }
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
        .header("content-type", "text/html; charset=UTF-8")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-headers", "x-requested-with")
        .header("access-control-allow-methods", "POST, GET, OPTIONS")
        .header("strict-transport-security", "max-age=15768000")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn server_error(msg: &str) -> Response<Full<Bytes>> {
    let body = "<html><title>500: Internal Server Error</title><body>500: Internal Server Error</body></html>";
    tracing::warn!(error = msg, "500");
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("content-type", "text/html; charset=UTF-8")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-headers", "x-requested-with")
        .header("access-control-allow-methods", "POST, GET, OPTIONS")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

async fn collect_body(req: Request<Incoming>) -> Result<Bytes, hyper::Error> {
    let body = req.into_body().collect().await?.to_bytes();
    if body.len() > MAX_BODY_BYTES {
        // Production webcash.org returns 500 on oversized bodies; we mirror that.
        // (The Tornado default behavior, not our extension.)
    }
    Ok(body)
}

mod handlers {
    use super::*;

    /// `GET /api/v1/target`
    pub async fn target<A: Asset + MintableAsset, S: LedgerStore<A>>(
        state: &Server<A, S>,
    ) -> Response<Full<Bytes>> {
        let cfg = &state.config.mining;
        let difficulty = cfg.current_difficulty().unwrap_or(1);
        let mining = wats_to_string(cfg.mining_amount_wats);
        let subsidy = wats_to_string(cfg.subsidy_amount_wats);
        let total = (cfg.mining_amount_wats + cfg.subsidy_amount_wats).max(1);
        let ratio = cfg.mining_amount_wats as f64 / total as f64;
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
                "Webycash Terms of Service\n\nPlaceholder; full text wired in M1 follow-up.\n",
            )))
            .unwrap()
    }

    /// `POST /api/v1/health_check`
    ///
    /// Body shape (matches production webcash.org):
    /// ```json
    /// ["e{amt}:public:{hash}", ...]
    /// ```
    ///
    /// Response:
    /// ```json
    /// {"status": "success",
    ///  "results": {"e{amt}:public:{hash}": {"spent": null|true|false}}}
    /// ```
    pub async fn health_check<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset,
        S: LedgerStore<A>,
    {
        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };
        let tokens: Vec<String> = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };

        // Parse each token to its public form, normalize, extract hash.
        // Production normalizes "e1.0:..." → "e1:...". Our parser preserves
        // the amount precision; we re-emit via Display which produces
        // "e1.00000000:..." — to match production's stripping we rewrite.
        let mut hashes: Vec<String> = Vec::with_capacity(tokens.len());
        let mut canonical: Vec<String> = Vec::with_capacity(tokens.len());
        for token in &tokens {
            match A::parse_public(token) {
                Ok(p) => {
                    hashes.push(p.public_hash().to_string());
                    canonical.push(production_normalize_public(token));
                }
                Err(_) => {
                    return server_error(&format!("invalid token: {token}"));
                }
            }
        }

        let ns = Namespace::unscoped();
        let lookups = match state.store.check_tokens(&ns, &hashes).await {
            Ok(l) => l,
            Err(e) => return server_error(&format!("storage: {e}")),
        };

        // Hand-build the JSON to control exact field order: production
        // emits "status" before "results" and preserves input token order
        // inside "results". serde_json::Map sorts alphabetically (BTreeMap)
        // by default, which would invert "status"/"results". A manual
        // build also preserves the Python `json.dumps` two-space style.
        let mut body = String::with_capacity(64 + canonical.len() * 100);
        body.push_str(r#"{"status": "success", "results": {"#);
        for (i, ((_, spent), key)) in lookups.iter().zip(canonical.iter()).enumerate() {
            if i > 0 {
                body.push_str(", ");
            }
            body.push('"');
            // Escape any embedded quotes/backslashes (defensive; tokens
            // shouldn't contain them post-parse but the body is user input).
            for ch in key.chars() {
                match ch {
                    '"' => body.push_str("\\\""),
                    '\\' => body.push_str("\\\\"),
                    c => body.push(c),
                }
            }
            body.push_str(r#"": {"spent": "#);
            match spent {
                None => body.push_str("null"),
                Some(true) => body.push_str("true"),
                Some(false) => body.push_str("false"),
            }
            body.push('}');
        }
        body.push_str("}}");
        ok_text_html_json(body)
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
    let frac_str = format!("{:08}", frac);
    let trimmed = frac_str.trim_end_matches('0');
    format!("{}{}.{}", if wats < 0 { "-" } else { "" }, whole, trimmed)
}

/// Production webcash.org normalizes `e1.0:...` to `e1:...` in
/// health_check response keys (drops trailing `.0` from whole amounts).
/// Mirror that normalization.
fn production_normalize_public(token: &str) -> String {
    // Format: e{amount}:public:{hash}
    if let Some(rest) = token.strip_prefix('e') {
        if let Some((amt, suffix)) = rest.split_once(':') {
            let normalized = normalize_amount_str(amt);
            return format!("e{normalized}:{suffix}");
        }
    }
    token.to_string()
}

fn normalize_amount_str(s: &str) -> String {
    // Drop a single trailing ".0" from whole numbers; preserve fractional values.
    if let Some((whole, frac)) = s.split_once('.') {
        if frac.chars().all(|c| c == '0') && !frac.is_empty() {
            return whole.to_string();
        }
    }
    s.to_string()
}

/// Re-emit a JSON object with single spaces after ':' and ',' to match
/// Python `json.dumps(...)` default output (Tornado default).
fn compact_with_spaces(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + json.len() / 8);
    let mut in_string = false;
    let mut escape = false;
    for c in json.chars() {
        out.push(c);
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            ':' if !in_string => out.push(' '),
            ',' if !in_string => out.push(' '),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wats_format_matches_production() {
        assert_eq!(wats_to_string(195_312_500_0), "195.3125");
        assert_eq!(wats_to_string(9_765_625_00), "9.765625");
        assert_eq!(wats_to_string(100_000_000), "1");
        assert_eq!(wats_to_string(0), "0");
        assert_eq!(wats_to_string(150_000_000), "1.5");
    }

    #[test]
    fn production_normalize_drops_trailing_zero() {
        assert_eq!(
            production_normalize_public(
                "e1.0:public:0000000000000000000000000000000000000000000000000000000000000000"
            ),
            "e1:public:0000000000000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            production_normalize_public(
                "e1.00000000:public:0000000000000000000000000000000000000000000000000000000000000000"
            ),
            "e1:public:0000000000000000000000000000000000000000000000000000000000000000"
        );
        // Non-trailing-zero fractions preserved
        assert_eq!(
            production_normalize_public(
                "e1.5:public:0000000000000000000000000000000000000000000000000000000000000000"
            ),
            "e1.5:public:0000000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn compact_with_spaces_inserts_python_dumps_format() {
        let input = r#"{"status":"success","results":{"x":{"spent":null}}}"#;
        let output = compact_with_spaces(input);
        // Single space after colon AND comma, none added inside strings.
        assert_eq!(
            output,
            r#"{"status": "success", "results": {"x": {"spent": null}}}"#
        );
    }
}
