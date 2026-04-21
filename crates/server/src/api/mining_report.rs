use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;

use super::router::{bad_request, ok_json};
use super::{handler, validate, AppState, MAX_BODY_SIZE};

#[derive(Deserialize)]
struct MiningReportRequest {
    preimage: String,
    legalese: Legalese,
}

#[derive(Deserialize)]
struct Legalese {
    terms: bool,
}

// Normal mining report — returns JSON result.
handler!(MiningReportRequest, |state, req| {
    validate!(req.legalese.terms, "terms must be accepted");
    match state
        .server
        .miner()
        .submit_mining_report(req.preimage)
        .await
    {
        Ok(result) => ok_json(
            serde_json::json!({
                "status": "success",
                "difficulty_target": result.difficulty_target,
            })
            .to_string(),
        ),
        Err(e) => bad_request(&e.to_string()),
    }
});

/// Streaming mining report — returns SSE events.
pub async fn handle_stream(
    state: Arc<AppState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let collected = http_body_util::Limited::new(req.into_body(), MAX_BODY_SIZE)
        .collect()
        .await;
    let body_bytes = match collected {
        Ok(c) => c.to_bytes(),
        Err(_) => {
            return sse_response(&[
                r#"{"event":"rejected","error":"request body too large or invalid"}"#,
            ])
        }
    };

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

    match state
        .server
        .miner()
        .submit_mining_report(request.preimage)
        .await
    {
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
        .map(|data| format!("data: {data}\n\n"))
        .collect();
    // CORS headers added by Cors middleware in dispatch stack
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}
