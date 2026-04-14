use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response};

use super::AppState;
use super::router::{internal_error, ok_json};

pub async fn handle(
    state: Arc<AppState>,
    _req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    match state.server.miner().get_target().await {
        Ok(target) => {
            let body = serde_json::json!({
                "difficulty_target_bits": target.difficulty_target_bits,
                "epoch": target.epoch,
                "mining_amount": target.mining_amount.to_string(),
                "mining_subsidy_amount": target.mining_subsidy_amount.to_string(),
                "ratio": target.ratio,
            });
            ok_json(body.to_string())
        }
        Err(e) => internal_error(&e.to_string()),
    }
}
