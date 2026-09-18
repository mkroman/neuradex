//! The HTTP server.
//!
//! [`serve`] starts the axum server; the application state and its clients are constructed by
//! the API layer (see [`crate::api::v1::AppState`]).

use std::net::SocketAddr;

use crate::api::v1::AppState;

/// Starts the axum server on `addr` and drives it until a shutdown signal is received.
///
/// # Errors
///
/// Returns an error if the server fails to bind or serve.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let router = crate::api::v1::router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(%addr, "listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
}

/// Resolves when a shutdown signal is received (ctrl-c or SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("could not install ctrl-c handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("could not install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
