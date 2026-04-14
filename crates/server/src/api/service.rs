use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response};

/// A composable service trait for request handling.
///
/// Inspired by tower::Service but simplified for our use case:
/// no poll_ready, no backpressure -- just async call.
#[async_trait::async_trait]
pub trait Service<Req: Send>: Send + Sync {
    type Response;
    type Error;
    async fn call(&self, req: Req) -> Result<Self::Response, Self::Error>;
}

/// Our standard HTTP request/response types.
pub type HttpRequest = Request<Incoming>;
pub type HttpResponse = Response<Full<Bytes>>;

// ---------------------------------------------------------------------------
// Middleware: Logged
// ---------------------------------------------------------------------------

/// Wraps any service with tracing: logs the request path and response status.
pub struct Logged<S> {
    inner: S,
    label: &'static str,
}

impl<S> Logged<S> {
    pub fn new(inner: S, label: &'static str) -> Self {
        Self { inner, label }
    }
}

#[async_trait::async_trait]
impl<S> Service<HttpRequest> for Logged<S>
where
    S: Service<HttpRequest, Response = HttpResponse, Error = hyper::Error> + Send + Sync,
{
    type Response = HttpResponse;
    type Error = hyper::Error;

    async fn call(&self, req: HttpRequest) -> Result<Self::Response, Self::Error> {
        let method = req.method().clone();
        let path = req.uri().path().to_string();

        tracing::info!(
            label = self.label,
            %method,
            %path,
            "request received"
        );

        let response = self.inner.call(req).await?;

        tracing::info!(
            label = self.label,
            %method,
            %path,
            status = response.status().as_u16(),
            "response sent"
        );

        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Middleware: Timed
// ---------------------------------------------------------------------------

/// Wraps any service with duration tracking. Emits a tracing event with
/// the elapsed time for each request.
pub struct Timed<S> {
    inner: S,
    label: &'static str,
}

impl<S> Timed<S> {
    pub fn new(inner: S, label: &'static str) -> Self {
        Self { inner, label }
    }
}

#[async_trait::async_trait]
impl<S> Service<HttpRequest> for Timed<S>
where
    S: Service<HttpRequest, Response = HttpResponse, Error = hyper::Error> + Send + Sync,
{
    type Response = HttpResponse;
    type Error = hyper::Error;

    async fn call(&self, req: HttpRequest) -> Result<Self::Response, Self::Error> {
        let start = std::time::Instant::now();
        let path = req.uri().path().to_string();
        let result = self.inner.call(req).await;
        let elapsed = start.elapsed();

        tracing::info!(
            label = self.label,
            %path,
            elapsed_ms = elapsed.as_millis() as u64,
            "request completed"
        );

        result
    }
}

// ---------------------------------------------------------------------------
// Concrete handler service: wraps a handler fn into a Service impl.
// ---------------------------------------------------------------------------

use super::AppState;

/// A Service backed by a handler function.
pub struct HandlerService<F> {
    state: Arc<AppState>,
    handler: F,
}

impl<F> HandlerService<F> {
    pub fn new(state: Arc<AppState>, handler: F) -> Self {
        Self { state, handler }
    }
}

#[async_trait::async_trait]
impl<F, Fut> Service<HttpRequest> for HandlerService<F>
where
    F: Fn(Arc<AppState>, HttpRequest) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<HttpResponse, hyper::Error>> + Send,
{
    type Response = HttpResponse;
    type Error = hyper::Error;

    async fn call(&self, req: HttpRequest) -> Result<Self::Response, Self::Error> {
        (self.handler)(self.state.clone(), req).await
    }
}

