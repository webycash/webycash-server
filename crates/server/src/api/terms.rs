use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

use super::AppState;

const TERMS_TEXT: &str = r#"Webcash Terms of Service

By using this webcash server, you agree to the following terms:

1. Webcash tokens are bearer instruments. Loss of a token means loss of funds.
2. The server operator makes no guarantees about uptime or availability.
3. This software is provided "as is" under the MIT license.
4. You are responsible for securing your own tokens and private keys.
5. The server operator is not responsible for any losses incurred.

For testnet usage: tokens have no monetary value and are for testing only.
"#;

pub async fn handle(
    _state: Arc<AppState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let content_type = if req.uri().path().ends_with("/text") {
        "text/plain"
    } else {
        "text/html"
    };

    let body = if content_type == "text/html" {
        format!(
            "<!DOCTYPE html><html><head><title>Terms of Service</title></head><body><pre>{}</pre></body></html>",
            TERMS_TEXT
        )
    } else {
        TERMS_TEXT.to_string()
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", content_type)
        .header("cache-control", "public, max-age=7200")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}
