use std::collections::HashMap;

use super::handler;
use super::router::{bad_request, internal_error, ok_json};

handler!(Vec<String>, |state, hashes| {
    if hashes.is_empty() {
        return bad_request("empty hash list");
    }

    match state.server.ledger().health_check(hashes).await {
        Ok(results) => {
            let map: HashMap<String, serde_json::Value> = results
                .into_iter()
                .map(|(hash, spent, amount)| {
                    (
                        hash,
                        serde_json::json!({ "spent": spent, "amount": amount }),
                    )
                })
                .collect();
            ok_json(serde_json::json!({ "status": "success", "results": map }).to_string())
        }
        Err(e) => internal_error(&e.to_string()),
    }
});
