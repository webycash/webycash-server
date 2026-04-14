use std::sync::Arc;

use bytes::Bytes;
use http::Method;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

use super::AppState;
use super::service::{HandlerService, Logged, Service, Timed};

type BoxBody = Full<Bytes>;

fn json_response(status: StatusCode, body: &str) -> Result<Response<BoxBody>, hyper::Error> {
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "GET, POST, OPTIONS")
        .header("access-control-allow-headers", "content-type")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap())
}

pub fn not_found() -> Result<Response<BoxBody>, hyper::Error> {
    json_response(StatusCode::NOT_FOUND, r#"{"error":"not found"}"#)
}

pub fn method_not_allowed() -> Result<Response<BoxBody>, hyper::Error> {
    json_response(
        StatusCode::METHOD_NOT_ALLOWED,
        r#"{"error":"method not allowed"}"#,
    )
}

pub fn ok_json(body: String) -> Result<Response<BoxBody>, hyper::Error> {
    json_response(StatusCode::OK, &body)
}

pub fn bad_request(msg: &str) -> Result<Response<BoxBody>, hyper::Error> {
    let body = serde_json::json!({"error": msg}).to_string();
    json_response(StatusCode::BAD_REQUEST, &body)
}

pub fn internal_error(msg: &str) -> Result<Response<BoxBody>, hyper::Error> {
    let body = serde_json::json!({"error": msg}).to_string();
    json_response(StatusCode::INTERNAL_SERVER_ERROR, &body)
}

/// Route incoming requests to the appropriate handler.
///
/// Each endpoint is wrapped through the Service middleware stack:
/// Timed(Logged(HandlerService(handler_fn)))
pub async fn route(
    state: Arc<AppState>,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>, hyper::Error> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    // Handle CORS preflight
    if method == Method::OPTIONS {
        return json_response(StatusCode::NO_CONTENT, "");
    }

    match (method, path.as_str()) {
        (Method::GET, "/api/v1/target") => {
            dispatch(state, req, super::target::handle, "target").await
        }
        (Method::GET, "/api/v1/stats") => {
            dispatch(state, req, super::stats::handle, "stats").await
        }
        (Method::GET, "/terms") | (Method::GET, "/terms/text") => {
            dispatch(state, req, super::terms::handle, "terms").await
        }
        (Method::GET, "/api/v1/health") => {
            ok_json(r#"{"status":"ok","service":"webycash-server"}"#.to_string())
        }

        (Method::POST, "/api/v1/mining_report") => {
            dispatch(state, req, super::mining_report::handle, "mining_report").await
        }
        (Method::POST, "/api/v1/mining_report/stream") => {
            dispatch(state, req, super::mining_report::handle_stream, "mining_report_stream").await
        }
        (Method::POST, "/api/v1/replace") => {
            dispatch(state, req, super::replace::handle, "replace").await
        }
        (Method::POST, "/api/v1/health_check") => {
            dispatch(state, req, super::health_check::handle, "health_check").await
        }
        (Method::POST, "/api/v1/burn") => {
            dispatch(state, req, super::burn::handle, "burn").await
        }

        _ => not_found(),
    }
}

/// Dispatch a request through the Timed -> Logged -> Handler service stack.
async fn dispatch<F, Fut>(
    state: Arc<AppState>,
    req: Request<Incoming>,
    handler: F,
    label: &'static str,
) -> Result<Response<BoxBody>, hyper::Error>
where
    F: Fn(Arc<AppState>, Request<Incoming>) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<Response<BoxBody>, hyper::Error>> + Send,
{
    let svc = HandlerService::new(state, handler);
    let logged = Logged::new(svc, label);
    let timed = Timed::new(logged, label);
    timed.call(req).await
}
