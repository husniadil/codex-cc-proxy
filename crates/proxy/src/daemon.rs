//! Binding and serving.

use crate::error::ProxyError;
use crate::ingress::AppState;
use crate::ingress::router;
use std::net::Ipv4Addr;
use std::net::SocketAddr;

/// Bind loopback, and only loopback.
///
/// The daemon performs no authentication, which is safe precisely because every
/// caller reaching the socket is already a local process running as the user.
/// Binding any other address removes the assumption the whole security posture
/// rests on, so it is not configurable (§6 of `CLAUDE.md`).
pub async fn bind(port: u16) -> Result<tokio::net::TcpListener, ProxyError> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

    tokio::net::TcpListener::bind(addr).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            // Naming the conflict rather than selecting another port: a second
            // daemon on a different port is silently unused by a client already
            // configured for the first (`api.md` §1).
            return ProxyError::invalid_request(format!(
                "port {port} is already in use. Another daemon is probably running; \
                 stop it, or choose a different port and update the client's base URL."
            ));
        }
        ProxyError::invalid_request(format!("could not bind 127.0.0.1:{port}: {error}"))
    })
}

/// Serve until the process is asked to stop.
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> Result<(), ProxyError> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown())
        .await
        .map_err(|error| ProxyError::invalid_request(format!("server stopped: {error}")))
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
