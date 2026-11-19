use std::{io, process::ExitCode};

use axum::{
    Router,
    extract::Request,
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use tokio::net::TcpListener;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tracing::{Instrument, Level};
use tracing_subscriber::{filter::Targets, prelude::*};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::registry()
        .with(
            Targets::new()
                .with_default(Level::WARN)
                .with_target("rusttalk_", Level::INFO),
        )
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    if let Err(error) = serve().await {
        tracing::error!(%error, "API server failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn serve() -> io::Result<()> {
    let app = Router::new()
        .route("/v1/health", get(|| async { "ok\n" }))
        .layer(middleware::from_fn(log_request))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .layer(middleware::map_request(|mut request: Request| async {
            request.headers_mut().remove("x-request-id");
            request
        }));

    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!("API listening on port 8080");
    axum::serve(listener, app).await
}

async fn log_request(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|id| id.to_str().ok());
    let span = tracing::info_span!("request", request_id);
    async move {
        let response = next.run(request).await;
        tracing::info!(status = response.status().as_u16(), "request completed");
        response
    }
    .instrument(span)
    .await
}
