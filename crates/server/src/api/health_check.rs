use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};

use super::router::{bad_request, internal_error, ok_json};
use super::AppState;

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

    let hashes: Vec<String> = match serde_json::from_slice(&body_bytes) {
        Ok(h) => h,
        Err(e) => return bad_request(&format!("invalid request: {}", e)),
    };

    if hashes.is_empty() {
        return bad_request("empty hash list");
    }

    match state.server.ledger().health_check(hashes).await {
        Ok(results) => {
            let mut map = HashMap::new();
            for (hash, spent, amount) in results {
                let entry = serde_json::json!({
                    "spent": spent,
                    "amount": amount,
                });
                map.insert(hash, entry);
            }
            let body = serde_json::json!({
                "status": "success",
                "results": map,
            });
            ok_json(body.to_string())
        }
        Err(e) => internal_error(&e.to_string()),
    }
}
