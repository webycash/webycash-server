pub mod burn;
pub mod health_check;
pub mod mining_report;
pub mod replace;
pub mod router;
pub mod service;
pub mod stats;
pub mod target;
pub mod terms;

use crate::config::Config;
use crate::db::LedgerStore;
use crate::WebcashServer;

/// Maximum request body size (1 MB). Enforced by the handler! macro.
pub const MAX_BODY_SIZE: usize = 1_048_576;

/// Shared application state passed to all handlers.
pub struct AppState<S: LedgerStore = Box<dyn LedgerStore>> {
    pub server: WebcashServer<S>,
    pub config: Config,
}

/// Declarative POST handler. Parses body with size limit, deserializes,
/// then executes the handler body. Eliminates all body-collection boilerplate.
macro_rules! handler {
    ($req_ty:ty, |$state:ident, $req:ident| $body:expr) => {
        pub async fn handle(
            $state: std::sync::Arc<super::AppState>,
            req: hyper::Request<hyper::body::Incoming>,
        ) -> Result<hyper::Response<http_body_util::Full<bytes::Bytes>>, hyper::Error> {
            use http_body_util::BodyExt;

            let collected = http_body_util::Limited::new(req.into_body(), super::MAX_BODY_SIZE)
                .collect()
                .await;
            let body_bytes = match collected {
                Ok(c) => c.to_bytes(),
                Err(_) => return super::router::bad_request("request body too large or invalid"),
            };
            let $req: $req_ty = match serde_json::from_slice(&body_bytes) {
                Ok(r) => r,
                Err(e) => return super::router::bad_request(&format!("invalid request: {e}")),
            };
            $body
        }
    };
}

/// Declarative validation. Returns bad_request on failure.
macro_rules! validate {
    ($cond:expr, $msg:expr) => {
        if !($cond) {
            return super::router::bad_request($msg);
        }
    };
}

pub(crate) use handler;
pub(crate) use validate;
