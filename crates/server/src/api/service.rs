use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response};

/// A composable service trait for request handling.
///
/// Inspired by tower::Service but simplified: no poll_ready, no backpressure.
#[async_trait::async_trait]
pub trait Service<Req: Send>: Send + Sync {
    type Response;
    type Error;
    async fn call(&self, req: Req) -> Result<Self::Response, Self::Error>;
}

/// Standard HTTP request/response types.
pub type HttpRequest = Request<Incoming>;
pub type HttpResponse = Response<Full<Bytes>>;

// ---------------------------------------------------------------------------
// Middleware: Cors — adds CORS headers to all responses
// ---------------------------------------------------------------------------

pub struct Cors<S> {
    inner: S,
    origin: String,
}

impl<S> Cors<S> {
    pub fn new(inner: S, origin: String) -> Self {
        Self { inner, origin }
    }
}

#[async_trait::async_trait]
impl<S> Service<HttpRequest> for Cors<S>
where
    S: Service<HttpRequest, Response = HttpResponse, Error = hyper::Error> + Send + Sync,
{
    type Response = HttpResponse;
    type Error = hyper::Error;

    async fn call(&self, req: HttpRequest) -> Result<Self::Response, Self::Error> {
        let response = self.inner.call(req).await?;
        // Direct header injection — no response rebuild, no header copying
        let (mut parts, body) = response.into_parts();
        parts
            .headers
            .insert("access-control-allow-origin", self.origin.parse().unwrap());
        parts.headers.insert(
            "access-control-allow-methods",
            "GET, POST, OPTIONS".parse().unwrap(),
        );
        parts.headers.insert(
            "access-control-allow-headers",
            "content-type".parse().unwrap(),
        );
        Ok(Response::from_parts(parts, body))
    }
}

// ---------------------------------------------------------------------------
// Middleware: Logged — traces request/response
// ---------------------------------------------------------------------------

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
// Middleware: Timed — measures request duration
// ---------------------------------------------------------------------------

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

        tracing::info!(
            label = self.label,
            %path,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "request completed"
        );

        result
    }
}

// ---------------------------------------------------------------------------
// HandlerService: wraps a handler fn into a Service
// ---------------------------------------------------------------------------

use super::AppState;

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
