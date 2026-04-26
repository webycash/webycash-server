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
use webycash_asset_core::{
    Amount, Asset, AssetPublic, CollectibleRecordBuilder, IssuedAsset, MintableAsset, RecordBuilder,
    RecordOrigin, SplittableAsset, TransferableAsset,
};
use webycash_auth::{IssuerRegistry, NonceCache};
use webycash_mining::MiningConfig;
use webycash_asset_core::{ContractId, PgpFingerprint};
use webycash_storage::{
    BurnRecord, HashRecord, LedgerStore, Namespace, ReplaceOp, ReplaceResult, ReplacementRecord,
};

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
///
/// The server is a **single-use-seal registry**, not a contract validator.
/// Per RGB's design, contract execution is client-side: the wallet runs
/// the compiled AluVM library against the intended state transition
/// before submitting `/replace`. The server's job is to atomically
/// `(verify input public_hash exists + unspent) -> (mark spent + insert
/// output)` within a single `(contract_id, issuer_fp)` namespace, plus
/// enforce amount conservation for splittable assets.
///
/// Whoever holds the secret owns the asset — the server witnesses the
/// transfer of that ownership.
pub struct Server<A: Asset, S: LedgerStore<A>> {
    pub config: ServeConfig,
    pub store: Arc<S>,
    /// Optional issuer registry. When set, `/api/v1/issue` is enabled and
    /// validates each request's `X-Issuer-Signature`. Webcash leaves this
    /// `None`; RGB and Voucher binaries populate it from env-loaded keys.
    pub issuers: Option<Arc<IssuerRegistry>>,
    pub nonces: Arc<NonceCache>,
    _ph: PhantomData<A>,
}

impl<A: Asset, S: LedgerStore<A>> Server<A, S> {
    /// Build a fresh `Server` with the given config + storage backend.
    /// Issuers default to `None` — the binary calls `with_issuers` if
    /// `/api/v1/issue` should be enabled (RGB / Voucher path).
    pub fn new(config: ServeConfig, store: S) -> Self {
        Self {
            config,
            store: Arc::new(store),
            issuers: None,
            nonces: Arc::new(NonceCache::default()),
            _ph: PhantomData,
        }
    }

    /// Attach an `IssuerRegistry`. After this call, `/api/v1/issue`
    /// will accept signed mints from the registered fingerprints;
    /// without it, the handler returns 503.
    pub fn with_issuers(mut self, issuers: IssuerRegistry) -> Self {
        self.issuers = Some(Arc::new(issuers));
        self
    }
}


/// Bind to `config.bind_addr` and serve hyper requests until cancelled.
///
/// Builds with hyper-util's auto Builder so HTTP/1.1 keep-alive AND HTTP/2
/// are supported on the same port. Handles every Webcash-style endpoint;
/// `/api/v1/issue` is gated by `Server::issuers.is_some()` and
/// `A: IssuedAsset` (enforced by the `serve_issued` overload).
pub async fn serve<A, S>(server: Server<A, S>) -> anyhow::Result<()>
where
    A: Asset + MintableAsset + SplittableAsset + RecordBuilder,
    A::Record: HashRecord,
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

/// Variant of `serve` for issuer-namespaced asset flavors. Identical
/// routing plus `POST /api/v1/issue` (Ed25519-signed operator mint).
/// `Server::issuers` must be populated for `/issue` to accept anything;
/// otherwise the handler returns 503.
pub async fn serve_issued<A, S>(server: Server<A, S>) -> anyhow::Result<()>
where
    A: Asset + MintableAsset + SplittableAsset + RecordBuilder + IssuedAsset,
    A::Record: HashRecord,
    S: LedgerStore<A>,
{
    let addr = server.config.bind_addr;
    let state = Arc::new(server);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, asset = A::NAME, "listening (issued flavor)");

    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(route_issued::<A, S>(&state, req).await) }
            });
            let builder = Builder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection(TokioIo::new(stream), svc).await {
                tracing::warn!(%peer, error = %e, "connection error");
            }
        });
    }
}

async fn route_issued<A, S>(
    state: &Server<A, S>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>>
where
    A: Asset + MintableAsset + SplittableAsset + RecordBuilder + IssuedAsset,
    A::Record: HashRecord,
    S: LedgerStore<A>,
{
    match (req.method(), req.uri().path()) {
        (&Method::POST, "/api/v1/issue") => handlers::issue::<A, S>(state, req).await,
        _ => route::<A, S>(state, req).await,
    }
}

/// Variant for non-splittable / collectible asset flavors (RGB21 NFT).
/// Exposes the same endpoint surface as the splittable flavors —
/// every write goes through `/api/v1/replace` — but the handler
/// enforces a 1:1 arity (single input → single output, same
/// namespace) instead of a conservation law:
///   GET  /api/v1/target          (MintableAsset; reports difficulty)
///   POST /api/v1/health_check    (per-token namespace lookup)
///   POST /api/v1/replace         (1:1 ownership transfer; arity-checked)
///   POST /api/v1/burn
///   POST /api/v1/issue           (IssuedAsset, signed mint)
///   GET  /terms, /terms/text
///
/// `/api/v1/mining_report` is statically absent because RGB21 can't
/// be PoW-mined; issuance is operator-signed only.
pub async fn serve_collectible<A, S>(server: Server<A, S>) -> anyhow::Result<()>
where
    A: Asset + MintableAsset + TransferableAsset + IssuedAsset + CollectibleRecordBuilder,
    A::Record: HashRecord,
    S: LedgerStore<A>,
{
    let addr = server.config.bind_addr;
    let state = Arc::new(server);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, asset = A::NAME, "listening (collectible flavor)");
    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let state = state.clone();
                async move {
                    Ok::<_, Infallible>(route_collectible::<A, S>(&state, req).await)
                }
            });
            let builder = Builder::new(TokioExecutor::new());
            if let Err(e) = builder.serve_connection(TokioIo::new(stream), svc).await {
                tracing::warn!(%peer, error = %e, "connection error");
            }
        });
    }
}

async fn route_collectible<A, S>(
    state: &Server<A, S>,
    req: Request<Incoming>,
) -> Response<Full<Bytes>>
where
    A: Asset + MintableAsset + TransferableAsset + IssuedAsset + CollectibleRecordBuilder,
    A::Record: HashRecord,
    S: LedgerStore<A>,
{
    // Non-splittable flavors expose the SAME endpoint names as splittable
    // ones. Servers always replace secrets; the difference is the
    // arity constraint (1:1 here, N:M for splittable).
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/api/v1/target") => handlers::target::<A, S>(state).await,
        (&Method::GET, "/terms") | (&Method::GET, "/terms/text") => handlers::terms().await,
        (&Method::POST, "/api/v1/health_check") => {
            handlers::health_check_collectible::<A, S>(state, req).await
        }
        (&Method::POST, "/api/v1/replace") => {
            handlers::replace_collectible::<A, S>(state, req).await
        }
        (&Method::POST, "/api/v1/burn") => {
            handlers::burn_collectible::<A, S>(state, req).await
        }
        (&Method::POST, "/api/v1/issue") => {
            handlers::issue_collectible::<A, S>(state, req).await
        }
        _ => not_found(),
    }
}

async fn route<A, S>(state: &Server<A, S>, req: Request<Incoming>) -> Response<Full<Bytes>>
where
    A: Asset + MintableAsset + SplittableAsset + RecordBuilder,
    A::Record: HashRecord,
    S: LedgerStore<A>,
{
    match (req.method(), req.uri().path()) {
        (&Method::GET, "/api/v1/target") => handlers::target::<A, S>(state).await,
        (&Method::GET, "/terms") | (&Method::GET, "/terms/text") => handlers::terms().await,
        (&Method::POST, "/api/v1/health_check") => {
            handlers::health_check::<A, S>(state, req).await
        }
        (&Method::POST, "/api/v1/replace") => handlers::replace::<A, S>(state, req).await,
        (&Method::POST, "/api/v1/mining_report") => {
            handlers::mining_report::<A, S>(state, req).await
        }
        (&Method::POST, "/api/v1/burn") => handlers::burn::<A, S>(state, req).await,
        (&Method::GET, "/api/v1/stats") => handlers::stats::<A, S>(state).await,
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
    let _ = MAX_BODY_BYTES;
    Ok(body)
}

/// Body envelope shared by replace + burn + mining_report.
/// `legalese.terms` must be `true` for any state-mutating endpoint.
#[derive(serde::Deserialize)]
struct Legalese {
    #[serde(default)]
    terms: bool,
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

    /// `GET /api/v1/stats`
    ///
    /// Webycash extension (production webcash.org returns 404). Reports
    /// EconomyStats derived from MiningState.
    pub async fn stats<A, S>(state: &Server<A, S>) -> Response<Full<Bytes>>
    where
        A: Asset + MintableAsset + SplittableAsset + RecordBuilder,
        A::Record: HashRecord,
        S: LedgerStore<A>,
    {
        let stats = state.store.get_stats().await.unwrap_or_default();
        let body = format!(
            "{{\"total_circulation\": \"{}\", \"mining_reports_count\": {}, \
             \"difficulty_target_bits\": {}, \"epoch\": {}, \
             \"mining_amount\": \"{}\", \"mining_subsidy_amount\": \"{}\"}}",
            wats_to_string(stats.total_circulation_wats),
            stats.mining_reports_count,
            stats.difficulty_target_bits,
            stats.epoch,
            wats_to_string(stats.mining_amount_wats),
            wats_to_string(stats.subsidy_amount_wats),
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
        A: Asset + SplittableAsset + RecordBuilder,
        A::Record: HashRecord,
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
        let mut publics: Vec<<A as Asset>::Public> = Vec::with_capacity(tokens.len());
        for token in &tokens {
            match A::parse_public(token) {
                Ok(p) => {
                    hashes.push(p.public_hash().to_string());
                    canonical.push(production_normalize_public(token));
                    publics.push(p);
                }
                Err(_) => {
                    return server_error(&format!("invalid token: {token}"));
                }
            }
        }

        // Determine namespace per token. For Webcash, all tokens share an
        // unscoped namespace. For RGB/Voucher, each token carries its own
        // (contract_id, issuer_fp); we group lookups by namespace so a
        // single health_check can span multiple compartments.
        let mut lookups: Vec<(String, Option<bool>)> = Vec::with_capacity(hashes.len());
        // Build (index, namespace, hash) triples then bucket by namespace.
        type IndexedHash = (usize, String);
        let mut by_ns: std::collections::HashMap<Namespace, Vec<IndexedHash>> =
            std::collections::HashMap::new();
        for (idx, (hash, public)) in hashes.iter().zip(publics.iter()).enumerate() {
            let ns = match A::public_namespace_envelope(public) {
                Some((c, i)) => {
                    Namespace::scoped(ContractId(c), PgpFingerprint(i))
                }
                None => Namespace::unscoped(),
            };
            by_ns.entry(ns).or_default().push((idx, hash.clone()));
        }
        // Resolve per-bucket; reorder back to input order.
        lookups.resize(hashes.len(), (String::new(), None));
        for (ns, items) in by_ns {
            let bucket_hashes: Vec<String> = items.iter().map(|(_, h)| h.clone()).collect();
            let res = match state.store.check_tokens(&ns, &bucket_hashes).await {
                Ok(r) => r,
                Err(e) => return server_error(&format!("storage: {e}")),
            };
            for ((orig_idx, _), (h, spent)) in items.into_iter().zip(res) {
                lookups[orig_idx] = (h, spent);
            }
        }

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

    /// `POST /api/v1/replace`
    ///
    /// Body shape:
    /// ```json
    /// {
    ///   "webcashes": ["e{amt}:secret:{hex}"],
    ///   "new_webcashes": ["e{amt}:secret:{hex}"],
    ///   "legalese": {"terms": true}
    /// }
    /// ```
    /// Production response on success: `{"status": "success"}`.
    /// Conservation law enforced: sum of input amounts == sum of output amounts.
    pub async fn replace<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + SplittableAsset + RecordBuilder,
        A::Record: HashRecord,
        S: LedgerStore<A>,
    {
        #[derive(serde::Deserialize)]
        struct ReplaceBody {
            #[serde(default)]
            webcashes: Vec<String>,
            #[serde(default)]
            new_webcashes: Vec<String>,
            #[serde(default)]
            legalese: Option<Legalese>,
        }

        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };
        let parsed: ReplaceBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };

        if !parsed
            .legalese
            .as_ref()
            .is_some_and(|l| l.terms)
        {
            // Production matches: legalese rejection returns 500. We
            // align with that for now; real production may return 400.
            return server_error("legalese.terms must be true");
        }

        // Empty replace is accepted (production quirk preserved).
        if parsed.webcashes.is_empty() && parsed.new_webcashes.is_empty() {
            return ok_text_html_json(r#"{"status": "success"}"#.to_string());
        }

        // Parse + sum inputs (read amounts off the public hash via parse_secret).
        let mut input_secrets = Vec::with_capacity(parsed.webcashes.len());
        let mut input_hashes = Vec::with_capacity(parsed.webcashes.len());
        let mut input_total = Amount::ZERO;
        for token in &parsed.webcashes {
            match A::parse_secret(token) {
                Ok(s) => {
                    let amt = A::amount(&s);
                    input_total = match input_total.checked_add(amt) {
                        Some(t) => t,
                        None => return server_error("input amount overflow"),
                    };
                    let public = A::to_public(&s);
                    input_hashes.push(public.public_hash().to_string());
                    input_secrets.push(s);
                }
                Err(e) => return server_error(&format!("invalid input token: {e}")),
            }
        }
        // Parse + sum outputs.
        let mut output_secrets = Vec::with_capacity(parsed.new_webcashes.len());
        let mut output_total = Amount::ZERO;
        for token in &parsed.new_webcashes {
            match A::parse_secret(token) {
                Ok(s) => {
                    let amt = A::amount(&s);
                    output_total = match output_total.checked_add(amt) {
                        Some(t) => t,
                        None => return server_error("output amount overflow"),
                    };
                    output_secrets.push(s);
                }
                Err(e) => return server_error(&format!("invalid output token: {e}")),
            }
        }
        if input_total != output_total {
            return server_error(&format!(
                "amount mismatch: inputs={input_total} outputs={output_total}"
            ));
        }

        // Determine namespace from the first input. All inputs/outputs must
        // share the same (contract_id, issuer_fp). For Webcash, this is
        // unscoped and the check is a no-op.
        let ns = match input_secrets.first().and_then(A::namespace_envelope) {
            Some((c, i)) => Namespace::scoped(ContractId(c), PgpFingerprint(i)),
            None => Namespace::unscoped(),
        };
        // Verify same-namespace invariant on every input + output.
        for s in input_secrets.iter().chain(output_secrets.iter()) {
            let secret_ns = match A::namespace_envelope(s) {
                Some((c, i)) => Namespace::scoped(ContractId(c), PgpFingerprint(i)),
                None => Namespace::unscoped(),
            };
            if secret_ns != ns {
                return server_error(
                    "namespace mismatch: all inputs and outputs must share the same (contract_id, issuer_fp)",
                );
            }
        }

        // Build the replace op: outputs become Replaced records.
        let outputs: Vec<A::Record> = output_secrets
            .iter()
            .map(|s| A::record_from_secret(s, RecordOrigin::Replaced))
            .collect();
        let output_hashes: Vec<String> =
            outputs.iter().map(|r| r.public_hash().to_string()).collect();
        let record = ReplacementRecord {
            id: uuid::Uuid::new_v4().to_string(),
            input_hashes: input_hashes.clone(),
            output_hashes: output_hashes.clone(),
            total_amount_wats: input_total.wats,
            created_at: chrono::Utc::now(),
        };
        let op = ReplaceOp {
            inputs: input_hashes,
            outputs,
            record,
        };
        let results = state.store.batch_replace(&ns, &[op]).await;
        match results.into_iter().next() {
            Some(ReplaceResult::Ok) => ok_text_html_json(r#"{"status": "success"}"#.to_string()),
            Some(ReplaceResult::Failed(e)) => server_error(&format!("replace failed: {e}")),
            None => server_error("no result from batch_replace"),
        }
    }

    /// `POST /api/v1/mining_report`
    ///
    /// Body shape:
    /// ```json
    /// {
    ///   "preimage": "<base64 OR raw JSON>",
    ///   "legalese": {"terms": true}
    /// }
    /// ```
    /// The preimage decodes to:
    /// ```json
    /// {"webcash": ["e{amt}:secret:..."], "subsidy": ["e{amt}:secret:..."],
    ///  "timestamp": int_or_float, "difficulty": target_bits}
    /// ```
    /// Server verifies SHA256(preimage_bytes) has >= difficulty leading zeros,
    /// inserts the webcash + subsidy outputs as Mined records.
    pub async fn mining_report<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + MintableAsset + SplittableAsset + RecordBuilder,
        A::Record: HashRecord,
        S: LedgerStore<A>,
    {
        #[derive(serde::Deserialize)]
        struct MiningReportBody {
            preimage: String,
            #[serde(default)]
            legalese: Option<Legalese>,
        }
        #[derive(serde::Deserialize)]
        struct MiningPreimage {
            #[serde(default)]
            webcash: Vec<String>,
            #[serde(default)]
            subsidy: Vec<String>,
            #[serde(default, deserialize_with = "deserialize_flexible_u64")]
            #[allow(dead_code)]
            timestamp: u64,
        }

        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };
        let report: MiningReportBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };
        if !report
            .legalese
            .as_ref()
            .is_some_and(|l| l.terms)
        {
            return server_error("legalese.terms must be true");
        }

        let cfg = &state.config.mining;
        let target_bits = match cfg.current_difficulty() {
            Some(d) => d,
            None => return server_error("mining is disabled on this server"),
        };

        // PoW check — SHA256 of the submitted preimage bytes (raw JSON or base64).
        if !webycash_mining::verify_pow(&report.preimage, target_bits) {
            return server_error(&format!(
                "proof-of-work below target ({target_bits} bits)"
            ));
        }

        // Decode preimage: try base64 first (GPU/C++ miner), fallback to raw JSON.
        let preimage_json = match base64_try_decode(&report.preimage) {
            Some(s) => s,
            None => report.preimage.clone(),
        };
        let preimage: MiningPreimage = match serde_json::from_str(&preimage_json) {
            Ok(p) => p,
            Err(e) => return server_error(&format!("invalid preimage: {e}")),
        };

        // Parse all webcash + subsidy outputs as MINED records.
        let mut records: Vec<A::Record> = Vec::with_capacity(
            preimage.webcash.len() + preimage.subsidy.len(),
        );
        for token in preimage.webcash.iter().chain(preimage.subsidy.iter()) {
            match A::parse_secret(token) {
                Ok(secret) => {
                    records.push(A::record_from_secret(&secret, RecordOrigin::Mined));
                }
                Err(e) => return server_error(&format!("invalid mined token: {e}")),
            }
        }

        if let Err(e) = state.store.insert_tokens(&records).await {
            return server_error(&format!("insert: {e}"));
        }

        // Update mining state: increment count, accrue circulation.
        let mut state_now = state
            .store
            .get_mining_state()
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        state_now.mining_reports_count = state_now.mining_reports_count.saturating_add(1);
        state_now.difficulty_target_bits = target_bits;
        state_now.mining_amount_wats = cfg.mining_amount_wats;
        state_now.subsidy_amount_wats = cfg.subsidy_amount_wats;
        let added: i64 = records
            .iter()
            .map(|r| {
                r.amount_wats()
            })
            .sum();
        state_now.total_circulation_wats = state_now.total_circulation_wats.saturating_add(added);
        let _ = state.store.update_mining_state(&state_now).await;

        ok_text_html_json(r#"{"status": "success"}"#.to_string())
    }

    /// `POST /api/v1/issue`
    ///
    /// Operator-private mint endpoint for issuer-namespaced asset flavors
    /// (RGB, Voucher). Body shape:
    /// ```json
    /// {
    ///   "issuer_fp": "<40-hex>",
    ///   "outputs": ["e{amt}:secret:{hex}:{contract}:{fp}", ...],
    ///   "nonce": "<unique>",
    ///   "ts": <unix_seconds>,
    ///   "legalese": {"terms": true}
    /// }
    /// ```
    /// Header: `X-Issuer-Signature: <hex 64-byte detached Ed25519 sig>`.
    /// The signature must be over the canonical request body (the JSON
    /// bytes as received). Server validates against the registered issuer
    /// pubkey, checks nonce isn't replayed, parses outputs, and inserts
    /// them as Issued records.
    pub async fn issue<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + SplittableAsset + RecordBuilder + IssuedAsset,
        A::Record: HashRecord,
        S: LedgerStore<A>,
    {
        let Some(registry) = state.issuers.clone() else {
            return server_error("issuer registry not configured");
        };

        // Pull the signature header BEFORE consuming the body.
        let sig_hex = match req
            .headers()
            .get("x-issuer-signature")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
        {
            Some(s) => s,
            None => return server_error("missing X-Issuer-Signature header"),
        };
        let sig_bytes = match hex::decode(&sig_hex) {
            Ok(b) => b,
            Err(e) => return server_error(&format!("signature hex: {e}")),
        };

        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };

        #[derive(serde::Deserialize)]
        struct IssueBody {
            issuer_fp: String,
            outputs: Vec<String>,
            nonce: String,
            #[serde(default)]
            #[allow(dead_code)]
            ts: u64,
            #[serde(default)]
            legalese: Option<Legalese>,
        }
        let parsed: IssueBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };
        if !parsed.legalese.as_ref().is_some_and(|l| l.terms) {
            return server_error("legalese.terms must be true");
        }

        let issuer = webycash_asset_core::PgpFingerprint(parsed.issuer_fp.to_lowercase());
        if let Err(e) = registry.verify(&issuer, &body, &sig_bytes) {
            return server_error(&format!("auth: {e}"));
        }
        if let Err(e) = state.nonces.check_and_insert(&issuer, &parsed.nonce) {
            return server_error(&format!("nonce: {e}"));
        }

        // Parse outputs, verify each lives in the SAME (contract_id, issuer_fp)
        // namespace as the request envelope.
        let mut secrets = Vec::with_capacity(parsed.outputs.len());
        for token in &parsed.outputs {
            match A::parse_secret(token) {
                Ok(s) => {
                    let parsed_issuer = A::issuer(&s);
                    if parsed_issuer != &issuer {
                        return server_error(
                            "output issuer fingerprint must match envelope issuer_fp",
                        );
                    }
                    secrets.push(s);
                }
                Err(e) => return server_error(&format!("invalid output: {e}")),
            }
        }
        // All outputs must share the same contract_id (envelope-level invariant).
        if let Some(first) = secrets.first() {
            let contract = A::contract_id(first).clone();
            for s in &secrets[1..] {
                if A::contract_id(s) != &contract {
                    return server_error("all outputs must share one contract_id");
                }
            }
        }

        // Build records; tag origin as Replaced (we don't have an Issued
        // RecordOrigin variant currently — voucher/RGB asset crates can
        // distinguish via a follow-up).
        let records: Vec<A::Record> = secrets
            .iter()
            .map(|s| A::record_from_secret(s, RecordOrigin::Replaced))
            .collect();
        if let Err(e) = state.store.insert_tokens(&records).await {
            return server_error(&format!("insert: {e}"));
        }

        ok_text_html_json(r#"{"status": "success"}"#.to_string())
    }

    /// `POST /api/v1/health_check` — collectible variant. Same contract as
    /// the splittable handler but uses `CollectibleRecordBuilder` for the
    /// namespace lookup.
    pub async fn health_check_collectible<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + TransferableAsset + CollectibleRecordBuilder,
        A::Record: HashRecord,
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

        let mut hashes: Vec<String> = Vec::with_capacity(tokens.len());
        let mut publics: Vec<<A as Asset>::Public> = Vec::with_capacity(tokens.len());
        for token in &tokens {
            match A::parse_public(token) {
                Ok(p) => {
                    hashes.push(p.public_hash().to_string());
                    publics.push(p);
                }
                Err(_) => return server_error(&format!("invalid token: {token}")),
            }
        }

        let mut lookups: Vec<(String, Option<bool>)> =
            vec![(String::new(), None); hashes.len()];
        let mut by_ns: std::collections::HashMap<Namespace, Vec<(usize, String)>> =
            std::collections::HashMap::new();
        for (idx, (hash, public)) in hashes.iter().zip(publics.iter()).enumerate() {
            let ns = match A::public_namespace_envelope(public) {
                Some((c, i)) => Namespace::scoped(ContractId(c), PgpFingerprint(i)),
                None => Namespace::unscoped(),
            };
            by_ns.entry(ns).or_default().push((idx, hash.clone()));
        }
        for (ns, items) in by_ns {
            let bucket: Vec<String> = items.iter().map(|(_, h)| h.clone()).collect();
            let res = match state.store.check_tokens(&ns, &bucket).await {
                Ok(r) => r,
                Err(e) => return server_error(&format!("storage: {e}")),
            };
            for ((orig_idx, _), (h, spent)) in items.into_iter().zip(res) {
                lookups[orig_idx] = (h, spent);
            }
        }

        let mut body = String::with_capacity(64 + tokens.len() * 100);
        body.push_str(r#"{"status": "success", "results": {"#);
        for (i, ((_, spent), key)) in lookups.iter().zip(tokens.iter()).enumerate() {
            if i > 0 {
                body.push_str(", ");
            }
            body.push('"');
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

    /// `POST /api/v1/replace` — non-splittable variant.
    ///
    /// Replace ONE secret with ONE new secret (RGB21 NFT). Servers
    /// always replace secrets; the non-splittable case is a 1:1
    /// constrained replace (no amount conservation, exactly one input,
    /// exactly one output). Same wire shape as the splittable replace
    /// for consistency:
    /// ```json
    /// {
    ///   "webcashes":     ["secret:{hex}:{contract}:{issuer}"],
    ///   "new_webcashes": ["secret:{hex2}:{contract}:{issuer}"],
    ///   "legalese": {"terms": true}
    /// }
    /// ```
    /// Server enforces same-namespace + 1:1 arity + atomically marks
    /// input spent and inserts output. (Real RGB transition validation
    /// happens client-side, before the wallet submits this request.)
    pub async fn replace_collectible<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + TransferableAsset + CollectibleRecordBuilder + IssuedAsset,
        A::Record: HashRecord,
        S: LedgerStore<A>,
    {
        #[derive(serde::Deserialize)]
        struct ReplaceBody {
            #[serde(default)]
            webcashes: Vec<String>,
            #[serde(default)]
            new_webcashes: Vec<String>,
            #[serde(default)]
            legalese: Option<Legalese>,
        }
        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };
        let parsed: ReplaceBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };
        if !parsed.legalese.as_ref().is_some_and(|l| l.terms) {
            return server_error("legalese.terms must be true");
        }
        // Non-splittable arity: exactly 1 input, exactly 1 output.
        if parsed.webcashes.len() != 1 || parsed.new_webcashes.len() != 1 {
            return server_error(
                "non-splittable replace requires exactly 1 input and 1 output",
            );
        }
        let input = match A::parse_secret(&parsed.webcashes[0]) {
            Ok(s) => s,
            Err(e) => return server_error(&format!("invalid input: {e}")),
        };
        let output = match A::parse_secret(&parsed.new_webcashes[0]) {
            Ok(s) => s,
            Err(e) => return server_error(&format!("invalid output: {e}")),
        };
        // Same-namespace check via the asset's IssuedAsset impl.
        if A::issuer(&input) != A::issuer(&output)
            || A::contract_id(&input) != A::contract_id(&output)
        {
            return server_error("namespace mismatch (input/output)");
        }
        // Type-level structural-validity (asset-specific). Real RGB
        // transition validation lives CLIENT-SIDE in the wallet
        // (webylib-wasm/contract.rs), where the contract bytecode and
        // ancestor state are accessible.
        if let Err(e) = A::validate_transfer(&input, &output) {
            return server_error(&format!("rejected: {e}"));
        }

        // Non-splittable: amount is conceptually 1; arity is 1:1.
        let ns = match <A as CollectibleRecordBuilder>::namespace_envelope(&input) {
            Some((c, i)) => Namespace::scoped(ContractId(c), PgpFingerprint(i)),
            None => Namespace::unscoped(),
        };
        let public = A::to_public(&input);
        let input_hash = public.public_hash().to_string();
        let output_record =
            <A as CollectibleRecordBuilder>::record_from_secret(&output, RecordOrigin::Replaced);
        let output_hash = output_record.public_hash().to_string();
        let record = ReplacementRecord {
            id: uuid::Uuid::new_v4().to_string(),
            input_hashes: vec![input_hash.clone()],
            output_hashes: vec![output_hash],
            total_amount_wats: 0, // NFTs have no fungible amount
            created_at: chrono::Utc::now(),
        };
        let op = ReplaceOp {
            inputs: vec![input_hash],
            outputs: vec![output_record],
            record,
        };
        let results = state.store.batch_replace(&ns, &[op]).await;
        match results.into_iter().next() {
            Some(ReplaceResult::Ok) => ok_text_html_json(r#"{"status": "success"}"#.to_string()),
            Some(ReplaceResult::Failed(e)) => server_error(&format!("replace failed: {e}")),
            None => server_error("no result"),
        }
    }

    /// `POST /api/v1/burn` — non-splittable variant.
    pub async fn burn_collectible<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + TransferableAsset + CollectibleRecordBuilder,
        S: LedgerStore<A>,
    {
        #[derive(serde::Deserialize)]
        struct BurnBody {
            webcash: String,
            #[serde(default)]
            legalese: Option<Legalese>,
        }
        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };
        let parsed: BurnBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };
        if !parsed.legalese.as_ref().is_some_and(|l| l.terms) {
            return server_error("legalese.terms must be true");
        }
        let secret = match A::parse_secret(&parsed.webcash) {
            Ok(s) => s,
            Err(e) => return server_error(&format!("invalid token: {e}")),
        };
        let public = A::to_public(&secret);
        let hash = public.public_hash().to_string();
        let record = BurnRecord {
            id: uuid::Uuid::new_v4().to_string(),
            public_hash: hash.clone(),
            amount_wats: 0,
            burned_at: chrono::Utc::now(),
        };
        let ns = match <A as CollectibleRecordBuilder>::namespace_envelope(&secret) {
            Some((c, i)) => Namespace::scoped(ContractId(c), PgpFingerprint(i)),
            None => Namespace::unscoped(),
        };
        match state.store.batch_burn(&ns, &[(hash, record)]).await {
            Ok(()) => ok_text_html_json(r#"{"status": "success"}"#.to_string()),
            Err(e) => server_error(&format!("burn failed: {e}")),
        }
    }

    /// `POST /api/v1/issue` — collectible variant. Operator-signed mint.
    /// Same envelope as the splittable issue but uses
    /// `CollectibleRecordBuilder` to build records.
    pub async fn issue_collectible<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + TransferableAsset + CollectibleRecordBuilder + IssuedAsset,
        A::Record: HashRecord,
        S: LedgerStore<A>,
    {
        let Some(registry) = state.issuers.clone() else {
            return server_error("issuer registry not configured");
        };
        let sig_hex = match req
            .headers()
            .get("x-issuer-signature")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
        {
            Some(s) => s,
            None => return server_error("missing X-Issuer-Signature header"),
        };
        let sig_bytes = match hex::decode(&sig_hex) {
            Ok(b) => b,
            Err(e) => return server_error(&format!("signature hex: {e}")),
        };
        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };
        #[derive(serde::Deserialize)]
        struct IssueBody {
            issuer_fp: String,
            outputs: Vec<String>,
            nonce: String,
            #[serde(default)]
            #[allow(dead_code)]
            ts: u64,
            #[serde(default)]
            legalese: Option<Legalese>,
        }
        let parsed: IssueBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };
        if !parsed.legalese.as_ref().is_some_and(|l| l.terms) {
            return server_error("legalese.terms must be true");
        }
        let issuer = webycash_asset_core::PgpFingerprint(parsed.issuer_fp.to_lowercase());
        if let Err(e) = registry.verify(&issuer, &body, &sig_bytes) {
            return server_error(&format!("auth: {e}"));
        }
        if let Err(e) = state.nonces.check_and_insert(&issuer, &parsed.nonce) {
            return server_error(&format!("nonce: {e}"));
        }
        let mut secrets = Vec::with_capacity(parsed.outputs.len());
        for token in &parsed.outputs {
            match A::parse_secret(token) {
                Ok(s) => {
                    if A::issuer(&s) != &issuer {
                        return server_error(
                            "output issuer fingerprint must match envelope issuer_fp",
                        );
                    }
                    secrets.push(s);
                }
                Err(e) => return server_error(&format!("invalid output: {e}")),
            }
        }
        let records: Vec<A::Record> = secrets
            .iter()
            .map(|s| {
                <A as CollectibleRecordBuilder>::record_from_secret(s, RecordOrigin::Replaced)
            })
            .collect();
        if let Err(e) = state.store.insert_tokens(&records).await {
            return server_error(&format!("insert: {e}"));
        }
        ok_text_html_json(r#"{"status": "success"}"#.to_string())
    }

    /// `POST /api/v1/burn`
    ///
    /// Body shape: `{"webcash": "e{amt}:secret:{hex}", "legalese": {"terms": true}}`.
    /// Marks the token as spent, records an audit entry.
    pub async fn burn<A, S>(
        state: &Server<A, S>,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>>
    where
        A: Asset + SplittableAsset + RecordBuilder,
        S: LedgerStore<A>,
    {
        #[derive(serde::Deserialize)]
        struct BurnBody {
            webcash: String,
            #[serde(default)]
            legalese: Option<Legalese>,
        }
        let body = match collect_body(req).await {
            Ok(b) => b,
            Err(e) => return server_error(&format!("body: {e}")),
        };
        let parsed: BurnBody = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return server_error(&format!("parse: {e}")),
        };
        if !parsed
            .legalese
            .as_ref()
            .is_some_and(|l| l.terms)
        {
            return server_error("legalese.terms must be true");
        }
        let secret = match A::parse_secret(&parsed.webcash) {
            Ok(s) => s,
            Err(e) => return server_error(&format!("invalid token: {e}")),
        };
        let amt = A::amount(&secret).wats;
        let public = A::to_public(&secret);
        let hash = public.public_hash().to_string();
        let record = BurnRecord {
            id: uuid::Uuid::new_v4().to_string(),
            public_hash: hash.clone(),
            amount_wats: amt,
            burned_at: chrono::Utc::now(),
        };
        let ns = match A::namespace_envelope(&secret) {
            Some((c, i)) => Namespace::scoped(ContractId(c), PgpFingerprint(i)),
            None => Namespace::unscoped(),
        };
        match state.store.batch_burn(&ns, &[(hash, record)]).await {
            Ok(()) => ok_text_html_json(r#"{"status": "success"}"#.to_string()),
            Err(e) => server_error(&format!("burn failed: {e}")),
        }
    }
}

fn base64_try_decode(s: &str) -> Option<String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
}

fn deserialize_flexible_u64<'de, D>(d: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().map(|f| f as u64))
            .ok_or_else(|| serde::de::Error::custom("non-numeric timestamp")),
        _ => Err(serde::de::Error::custom("timestamp must be a number")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wats_format_matches_production() {
        // 195.3125 webycash = 19_531_250_000 wats (8 decimal places)
        assert_eq!(wats_to_string(19_531_250_000), "195.3125");
        // 9.765625 webycash = 976_562_500 wats
        assert_eq!(wats_to_string(976_562_500), "9.765625");
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

}
