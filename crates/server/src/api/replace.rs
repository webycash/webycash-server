use serde::Deserialize;

use super::router::{bad_request, ok_json};
use super::{handler, validate};

#[derive(Deserialize)]
struct ReplaceRequest {
    webcashes: Vec<String>,
    new_webcashes: Vec<String>,
    legalese: Legalese,
}

#[derive(Deserialize)]
struct Legalese {
    terms: bool,
}

handler!(ReplaceRequest, |state, req| {
    validate!(req.legalese.terms, "terms must be accepted");
    validate!(!req.webcashes.is_empty(), "webcashes must not be empty");
    validate!(
        !req.new_webcashes.is_empty(),
        "new_webcashes must not be empty"
    );
    match state
        .server
        .batcher
        .replace(req.webcashes, req.new_webcashes)
        .await
    {
        Ok(()) => ok_json(r#"{"status":"success"}"#.to_string()),
        Err(e) => bad_request(&e.to_string()),
    }
});
