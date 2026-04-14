use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;

use super::AppState;
use super::router::{bad_request, ok_json};

#[derive(Deserialize)]
struct MiningReportRequest {
    preimage: String,
    legalese: Legalese,
}

#[derive(Deserialize)]
struct Legalese {
    terms: bool,
}

/// Mining report handler. Supports two modes:
/// - Normal: returns JSON `{"status":"success","difficulty_target":N}`
/// - Streaming (Accept: text/event-stream): SSE events for each validation stage
pub async fn handle(
    state: Arc<AppState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let body_bytes = req
        .collect()
        .await
        .map_err(|_| ())
        .unwrap_or_default()
        .to_bytes();

    let request: MiningReportRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => return bad_request(&format!("invalid request: {}", e)),
    };

    if !request.legalese.terms {
        return bad_request("terms must be accepted");
    }

    match state
        .server
        .miner()
        .submit_mining_report(request.preimage)
        .await
    {
        Ok(result) => {
            let body = serde_json::json!({
                "status": "success",
                "difficulty_target": result.difficulty_target,
            });
            ok_json(body.to_string())
        }
        Err(e) => bad_request(&e.to_string()),
    }
}

/// Streaming mining report handler — returns SSE events for validation stages.
/// Route: POST /api/v1/mining_report/stream
///
/// Events:
///   data: {"event":"validating","stage":"pow"}
///   data: {"event":"validating","stage":"preimage"}
///   data: {"event":"accepted","difficulty_target":16}
/// or:
///   data: {"event":"rejected","error":"..."}
pub async fn handle_stream(
    state: Arc<AppState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let body_bytes = req
        .collect()
        .await
        .map_err(|_| ())
        .unwrap_or_default()
        .to_bytes();

    let request: MiningReportRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => {
            return sse_response(&[&format!(
                r#"{{"event":"rejected","error":"invalid request: {}"}}"#,
                e.to_string().replace('"', "'")
            )]);
        }
    };

    if !request.legalese.terms {
        return sse_response(&[r#"{"event":"rejected","error":"terms not accepted"}"#]);
    }

    // Validate and submit
    let result = state
        .server
        .miner()
        .submit_mining_report(request.preimage)
        .await;

    match result {
        Ok(r) => sse_response(&[
            r#"{"event":"validating","stage":"pow"}"#,
            r#"{"event":"validating","stage":"preimage"}"#,
            r#"{"event":"validating","stage":"amounts"}"#,
            &format!(
                r#"{{"event":"accepted","difficulty_target":{}}}"#,
                r.difficulty_target
            ),
        ]),
        Err(e) => sse_response(&[&format!(
            r#"{{"event":"rejected","error":"{}"}}"#,
            e.to_string().replace('"', "'")
        )]),
    }
}

fn sse_response(events: &[&str]) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let body: String = events
        .iter()
        .map(|data| format!("data: {}\n\n", data))
        .collect();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("access-control-allow-origin", "*")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}
