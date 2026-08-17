//! `docs/proxy-behavior.md` §8 — completing an authorization.

use super::flow::Authorization;
use super::jwt;
use super::store::CredentialStore;
use super::store::Credentials;
use crate::error::ProxyError;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
}

/// Exchange an authorization code for a grant, and store it.
///
/// The state is checked before the code is spent. A response carrying the wrong
/// state is not this flow's response, and exchanging its code would attach
/// somebody else's authorization to this proxy.
pub async fn complete(
    client: &reqwest::Client,
    endpoint: &str,
    client_id: &str,
    authorization: &Authorization,
    returned_state: &str,
    code: &str,
    store: &Arc<dyn CredentialStore>,
) -> Result<Credentials, ProxyError> {
    if returned_state != authorization.state {
        return Err(ProxyError::authentication(
            "the authorization response did not match this login attempt; start again",
        ));
    }

    // Form-encoded. The refresh that follows later is JSON — they differ, and
    // sending the wrong one is rejected.
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", authorization.redirect_uri.as_str()),
        ("client_id", client_id),
        ("code_verifier", authorization.pkce.verifier()),
    ];

    let response = client
        .post(endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|error| {
            ProxyError::overloaded(format!("could not reach the authorization server: {error}"))
        })?;

    let status = response.status();
    let body = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(ProxyError::authentication(format!(
            "the authorization server refused the exchange ({status})"
        )));
    }

    let parsed: ExchangeResponse = serde_json::from_str(&body).map_err(|error| {
        // The message names the failure, never the body: it holds tokens.
        ProxyError::authentication(format!("unreadable token response: {error}"))
    })?;

    let credentials = Credentials {
        // Both come from claims rather than from response fields, because the
        // response has neither.
        expires_at: jwt::expiry(&parsed.access_token),
        account_id: jwt::account_id(parsed.id_token.as_deref()),
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
    };

    store.save(&credentials)?;
    Ok(credentials)
}

/// Pull `code` and `state` out of the callback query string.
pub fn parse_callback(query: &str) -> Result<(String, String), ProxyError> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;

    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = percent_decode(value);
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            "error_description" => description = Some(value),
            _ => {}
        }
    }

    // The server's own message is the only useful diagnostic when a user
    // declines or the request is rejected.
    if let Some(error) = error {
        let detail = description.unwrap_or_else(|| error.clone());
        return Err(ProxyError::authentication(format!(
            "authorization failed: {detail}"
        )));
    }

    match (code, state) {
        (Some(code), Some(state)) => Ok((code, state)),
        _ => Err(ProxyError::authentication(
            "the authorization response carried no code",
        )),
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes.get(index) {
            Some(b'%') => {
                let hex = value.get(index.saturating_add(1)..index.saturating_add(3));
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index = index.saturating_add(3);
                    }
                    None => {
                        out.push(b'%');
                        index = index.saturating_add(1);
                    }
                }
            }
            Some(b'+') => {
                out.push(b' ');
                index = index.saturating_add(1);
            }
            Some(byte) => {
                out.push(*byte);
                index = index.saturating_add(1);
            }
            None => break,
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Run a login end to end.
///
/// The callback server binds loopback on the fixed port the authorization
/// server already accepts as a redirect target, serves exactly one
/// authorization response, and stops. It is not a general-purpose server and
/// must not outlive the flow.
pub async fn run(
    store: Arc<dyn CredentialStore>,
    open_url: impl FnOnce(&str),
) -> Result<Credentials, ProxyError> {
    let authorization = super::flow::begin(super::flow::CALLBACK_PORT);

    let listener =
        tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, super::flow::CALLBACK_PORT))
            .await
            .map_err(|error| {
                ProxyError::invalid_request(format!(
                    "could not listen on port {} for the login callback: {error}. \
             The redirect target is fixed, so this port has to be free.",
                    super::flow::CALLBACK_PORT
                ))
            })?;

    open_url(&authorization.url);

    complete_from_listener(&listener, &authorization, &store).await
}

/// Wait for the one authorization response and exchange it.
///
/// Split out of `run` so the daemon can bind the port itself — reporting a port
/// it cannot take to the caller that asked, rather than surfacing it later as a
/// login that never completes — and then wait in a task of its own.
pub async fn complete_from_listener(
    listener: &tokio::net::TcpListener,
    authorization: &Authorization,
    store: &Arc<dyn CredentialStore>,
) -> Result<Credentials, ProxyError> {
    let query = accept_callback(listener).await?;
    let (code, state) = parse_callback(&query)?;

    complete(
        &reqwest::Client::new(),
        &super::flow::token_endpoint(),
        super::flow::CLIENT_ID,
        authorization,
        &state,
        &code,
        store,
    )
    .await
}

/// Read one request line, answer it, and return its query string.
async fn accept_callback(listener: &tokio::net::TcpListener) -> Result<String, ProxyError> {
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;

    loop {
        let (stream, _) = listener.accept().await.map_err(|error| {
            ProxyError::authentication(format!("the login callback failed: {error}"))
        })?;

        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        let Ok(Some(request_line)) = lines.next_line().await else {
            continue;
        };

        // `GET /auth/callback?code=... HTTP/1.1`
        let target = request_line.split_whitespace().nth(1).unwrap_or_default();
        let (path, query) = target.split_once('?').unwrap_or((target, ""));

        if path != super::flow::CALLBACK_PATH {
            let _ = writer
                .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n")
                .await;
            continue;
        }

        let outcome = if query.contains("error=") {
            "Authorization failed. You can close this tab and check the terminal."
        } else {
            "Signed in. You can close this tab."
        };
        let body = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{outcome}",
            outcome.len()
        );
        let _ = writer.write_all(body.as_bytes()).await;
        let _ = writer.flush().await;

        return Ok(query.to_owned());
    }
}
