use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use serde::Deserialize;

use super::router::{bad_request, ok_json};
use super::AppState;

#[derive(Deserialize)]
struct BurnRequest {
    destroy_webcash: Vec<String>,
    legalese: Legalese,
}

#[derive(Deserialize)]
struct Legalese {
    terms: bool,
}

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

    let request: BurnRequest = match serde_json::from_slice(&body_bytes) {
        Ok(r) => r,
        Err(e) => return bad_request(&format!("invalid request: {}", e)),
    };

    if !request.legalese.terms {
        return bad_request("terms must be accepted");
    }

    if request.destroy_webcash.is_empty() {
        return bad_request("destroy_webcash must not be empty");
    }

    match state.server.ledger().burn(request.destroy_webcash).await {
        Ok(()) => ok_json(r#"{"status":"success"}"#.to_string()),
        Err(e) => bad_request(&e.to_string()),
    }
}
