//! `docs/api.md` §3 — running the authorization flow from the control socket.
//!
//! Logging in used to belong to the CLI because the CLI owned the callback
//! port. That was a fact about where the code lived, not a constraint: the
//! redirect target is a fixed loopback port, and a long-lived loopback daemon
//! is a more natural host for it than a command that has to stay in the
//! foreground while the operator finds their browser.
//!
//! **Single-flight, because there is exactly one port.** A second `begin` would
//! either fail to bind or replace the state the first flow is waiting to match
//! — and the operator would be holding a URL whose callback is then rejected as
//! not matching this attempt. So a second caller joins the first and is told
//! so, rather than being handed a URL that cannot work.
//!
//! **An abandoned flow releases the port.** A browser that is never opened
//! would otherwise hold the port until the daemon stopped, and the next login
//! would fail to bind for a reason that has nothing to do with the next login.

use crate::auth::flow;
use crate::auth::login;
use crate::auth::store::CredentialStore;
use crate::error::ProxyError;
use std::sync::Arc;
use std::sync::Mutex;

/// How long an unopened flow holds the callback port.
///
/// Long enough to find a browser and sign in on a phone-based second factor,
/// short enough that an abandoned attempt is not still holding the port an hour
/// later.
const FLOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

struct InFlight {
    url: String,
    /// Dropping this cancels the task waiting on the callback, which drops the
    /// listener and frees the port.
    cancel: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
pub struct LoginFlow {
    current: Mutex<Option<InFlight>>,
}

/// What one `login` call answers with.
pub struct Started {
    pub url: String,
    pub already_in_flight: bool,
}

impl LoginFlow {
    /// Start a flow, or join the one already running.
    ///
    /// Binding happens here rather than in the spawned task so a port that
    /// cannot be taken is reported to the caller that asked, instead of
    /// surfacing later as a login that never completes.
    pub async fn start(
        self: &Arc<Self>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Result<Started, ProxyError> {
        if let Some(existing) = self.existing() {
            return Ok(Started {
                url: existing,
                already_in_flight: true,
            });
        }

        let authorization = flow::begin(flow::CALLBACK_PORT);
        let listener =
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, flow::CALLBACK_PORT))
                .await
                .map_err(|error| {
                    ProxyError::invalid_request(format!(
                        "could not listen on port {} for the login callback: {error}. \
                         The redirect target is fixed, so this port has to be free — \
                         another login, in this daemon or in the CLI, may be holding it.",
                        flow::CALLBACK_PORT
                    ))
                })?;

        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let url = authorization.url.clone();

        let flow_state = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = tokio::select! {
                result = login::complete_from_listener(&listener, &authorization, &credentials) => {
                    Some(result)
                }
                _ = cancelled => None,
                () = tokio::time::sleep(FLOW_TIMEOUT) => {
                    tracing::warn!(
                        "the login was abandoned before the browser came back; \
                         the callback port is free again"
                    );
                    None
                }
            };

            match outcome {
                Some(Ok(_)) => tracing::info!("login completed"),
                // Said out loud. The caller has already been answered, so a
                // silent failure here would leave `status` reporting not
                // connected with nothing anywhere explaining why.
                Some(Err(error)) => tracing::warn!(%error, "the login did not complete"),
                None => {}
            }

            flow_state.clear();
        });

        self.remember(InFlight {
            url: url.clone(),
            cancel,
        });

        Ok(Started {
            url,
            already_in_flight: false,
        })
    }

    /// Abandon the flow and release the port. Answering `false` when there was
    /// nothing to cancel, which is not an error — a caller cleaning up should
    /// not have to know whether it had started.
    pub fn cancel(&self) -> bool {
        match self.take() {
            Some(flow) => {
                // The receiver is dropped only when the task has already
                // finished, in which case the port is free anyway.
                let _ = flow.cancel.send(());
                true
            }
            None => false,
        }
    }

    fn existing(&self) -> Option<String> {
        self.current
            .lock()
            .ok()
            .and_then(|current| current.as_ref().map(|flow| flow.url.clone()))
    }

    fn remember(&self, flow: InFlight) {
        if let Ok(mut current) = self.current.lock() {
            *current = Some(flow);
        }
    }

    fn take(&self) -> Option<InFlight> {
        self.current.lock().ok().and_then(|mut c| c.take())
    }

    fn clear(&self) {
        if let Ok(mut current) = self.current.lock() {
            *current = None;
        }
    }
}
