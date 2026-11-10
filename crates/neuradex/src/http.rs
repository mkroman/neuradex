//! The HTTP server.
//!
//! [`serve`] starts the axum server; the application state and its clients are constructed at
//! startup (see [`crate::state::AppState`]).

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::time::Duration;

#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use crate::api::v1::router;
use crate::state::AppState;

/// The longest the server waits for in-flight requests to finish after a shutdown signal.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the content pipeline flushes sealed packs and objects while serving.
const CONTENT_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Starts the axum server on `addr` and drives it until a shutdown signal is received.
///
/// After the signal, in-flight requests are given ten seconds to finish. If they are still
/// running when that bound is reached, the server stops waiting for them and logs a warning.
/// When content storage is enabled, a background task flushes it periodically and one final
/// best-effort flush runs after the server stops.
///
/// # Errors
///
/// Returns an error if the server fails to bind or serve.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let flush_task = state
        .content
        .as_ref()
        .map(|content| content.spawn_flush_task(CONTENT_FLUSH_INTERVAL));
    let result = serve_inner(state, addr).await;

    if let Some(task) = flush_task {
        task.shutdown().await;
    }

    result
}

/// Binds the listener and drives the axum server until shutdown.
async fn serve_inner(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let router = router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!(
        "server.address" = %addr.ip(),
        "server.port" = addr.port(),
        "listening"
    );

    // The server's graceful-shutdown future waits on this channel instead of the signal
    // directly, so the drain can be timed from the moment the signal arrives.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.await;
        })
        .into_future();
    tokio::pin!(server);

    tokio::select! {
        result = &mut server => return result,
        () = shutdown_signal() => {}
    }

    let _ = shutdown_tx.send(());

    tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut server)
        .await
        .unwrap_or_else(|_| {
            tracing::warn!(
                "error.type" = "shutdown_timeout",
                "graceful shutdown timed out; abandoning remaining connections"
            );

            Ok(())
        })
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
        signal(SignalKind::terminate())
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
