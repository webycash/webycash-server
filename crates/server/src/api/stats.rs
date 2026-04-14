use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response};

use super::AppState;
use super::router::{internal_error, ok_json};
use crate::protocol::Amount;

pub async fn handle(
    state: Arc<AppState>,
    _req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    match state.server.stats().get_stats().await {
        Ok(stats) => {
            let body = serde_json::json!({
                "circulation": Amount::from_wats(stats.total_circulation_wats).to_string(),
                "mining_reports": stats.mining_reports_count,
                "difficulty_target_bits": stats.difficulty_target_bits,
                "epoch": stats.epoch,
                "mining_amount": Amount::from_wats(stats.mining_amount_wats).to_string(),
                "mining_subsidy_amount": Amount::from_wats(stats.subsidy_amount_wats).to_string(),
            });
            ok_json(body.to_string())
        }
        Err(e) => internal_error(&e.to_string()),
    }
}
