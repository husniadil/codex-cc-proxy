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

/// A stop asked for over the control socket.
///
/// **Two steps, and the order is the whole point.** `request` records the
/// intent and returns, so the handler can answer; the run loop is released only
/// once that answer has been written. A caller that saw its connection close
/// with no reply could not tell a clean stop from a crash, and learning what
/// happened is the reason to ask over the socket rather than send a signal.
///
/// Under a supervisor this is how a running daemon is replaced by the build on
/// disk: it stops, and the supervisor starts the file again. Whether anything
/// does that is the supervisor's business, so this reports only that it is
/// going.
#[derive(Debug, Default)]
pub struct Shutdown {
    requested: std::sync::atomic::AtomicBool,
    released: tokio::sync::Notify,
}

impl Shutdown {
    /// Record that a stop was asked for. Does not release anything yet.
    pub fn request(&self) {
        self.requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.requested.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Release the run loop. Called once the answer is on the wire.
    ///
    /// `notify_one` rather than `notify_waiters`: it stores a permit when
    /// nobody is waiting yet, so a stop asked for before the loop reaches its
    /// wait is not lost.
    pub fn release(&self) {
        self.released.notify_one();
    }

    pub async fn wait(&self) {
        self.released.notified().await;
    }
}
