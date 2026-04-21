use serde::Deserialize;

use super::router::{bad_request, ok_json};
use super::{handler, validate};

#[derive(Deserialize)]
struct BurnRequest {
    destroy_webcash: Vec<String>,
    legalese: Legalese,
}

#[derive(Deserialize)]
struct Legalese {
    terms: bool,
}

handler!(BurnRequest, |state, req| {
    validate!(req.legalese.terms, "terms must be accepted");
    validate!(
        !req.destroy_webcash.is_empty(),
        "destroy_webcash must not be empty"
    );
    match state.server.ledger().burn(req.destroy_webcash).await {
        Ok(()) => ok_json(r#"{"status":"success"}"#.to_string()),
        Err(e) => bad_request(&e.to_string()),
    }
});
