// webycash-server targets Linux and FreeBSD only.
// Compile-time gate: macOS/Windows builds emit a warning but still compile
// (needed for development). Production deployments must be Linux or FreeBSD.
#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
compile_error!("webycash-server only supports Linux and FreeBSD (macOS allowed for development)");

use std::net::SocketAddr;
use std::sync::Arc;

use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tracing_subscriber::EnvFilter;

use webycash_server::config::Config;
use webycash_server::WebcashServer;
use webycash_server::api;
use webycash_server::db;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("webycash_server=debug,hyper=info")),
        )
        .init();

    // Load config from CLI arg or environment
    let config = match std::env::args().nth(1) {
        Some(ref flag) if flag == "--config" => {
            let path = std::env::args()
                .nth(2)
                .expect("--config requires a path argument");
            Config::load(&path)?
        }
        _ => Config::from_env()?,
    };

    tracing::info!(
        mode = ?config.server.mode,
        backend = ?config.server.db.backend,
        difficulty = config.effective_difficulty(),
        "starting webycash-server"
    );

    // Create database backend
    let store = db::create_store(&config).await?;

    // Create and start server
    let mut server = WebcashServer::new(store, config.server.clone(), config.mining.clone());
    server.start().await?;

    let state = Arc::new(api::AppState {
        server,
        config: config.clone(),
    });

    // Bind and serve
    let addr: SocketAddr = config.server.bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                let state = state.clone();
                async move { api::router::route(state, req).await }
            });
            if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::new(),
            )
            .serve_connection(TokioIo::new(stream), service)
            .await
            {
                tracing::error!(%peer, error = %e, "connection error");
            }
        });
    }
}
