//! `docs/api.md` §3 — the control socket.

pub mod handler;
pub mod protocol;

use crate::error::ProxyError;
use handler::ControlState;
use protocol::Request;
use protocol::Response;
use protocol::codes;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

/// The most bytes a unix socket address can carry on this platform.
///
/// `sun_path` is 104 bytes on the BSDs and macOS and 108 on Linux, one of which
/// is the terminator. Nothing in the standard library exposes it, and exceeding
/// it fails at `bind` and at `connect` rather than at the point the path was
/// chosen.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub const PATH_LIMIT: usize = 103;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub const PATH_LIMIT: usize = 107;

/// Where the socket lives.
///
/// **One derivation, used by the daemon's bind and by every CLI call**, so the
/// pair cannot split: a CLI that derived its own path would talk to a daemon
/// that is not the one its configuration belongs to.
///
/// `PROXENOS_HOME` is what isolates a daemon from the operator's own, and the
/// socket is part of what has to move with it. When it named only the
/// configuration, an isolated CLI sharing a `TMPDIR` still reached the real
/// daemon — and every login path ends in `accounts.select` over that socket,
/// so an isolated login could switch the account the operator's daemon serves.
///
/// With no home named, the path is unchanged: an operator's running daemon is
/// addressed by the path it already bound.
pub fn default_path() -> PathBuf {
    path_for(
        std::env::var_os("PROXENOS_HOME").map(PathBuf::from).as_deref(),
        Some(std::env::temp_dir()).as_deref(),
    )
}

/// The same derivation, over an environment named rather than read.
///
/// **This exists so a supervisor unit and the operator's shell cannot split.**
/// A process launchd starts does not necessarily see the `TMPDIR` a login shell
/// does, and a daemon that binds one path while the CLI dials another comes up
/// healthy on the HTTP port while every verb reports connection refused. The
/// unit is rendered by putting the installing shell's values through this
/// function and carrying them explicitly, so both halves are the same call over
/// the same inputs rather than two derivations that happen to agree.
pub fn path_for(home: Option<&Path>, tmpdir: Option<&Path>) -> PathBuf {
    match home {
        Some(home) => home.join("proxenos.sock"),
        None => tmpdir
            .unwrap_or_else(|| Path::new("/tmp"))
            .join("proxenos.sock"),
    }
}

/// Refuse a path the platform cannot address, by name.
///
/// The silent variant of this costs a real debugging session: the bind fails
/// while the HTTP port comes up fine, so the daemon serves turns and looks
/// healthy while every CLI verb gets connection refused and `--detach` times
/// out waiting for a socket that will never appear. Both ends check, because
/// either can be the first to be given the path.
pub fn ensure_addressable(path: &Path) -> Result<(), ProxyError> {
    let length = path.as_os_str().len();
    if length <= PATH_LIMIT {
        return Ok(());
    }

    Err(ProxyError::invalid_request(format!(
        "the control socket path is {length} bytes, over the {PATH_LIMIT}-byte limit this \
         platform puts on a unix socket address: {}. Point `PROXENOS_HOME` at a shorter \
         directory, or unset it and use a shorter `TMPDIR`.",
        path.display()
    )))
}

/// This binary's version. One file is both the daemon and the CLI, so the
/// daemon answering a socket is not necessarily the build that asked.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Refuse a payload from a daemon that predates client policy.
///
/// **Read as a capability, not a version comparison.** Comparing version
/// strings forces a policy about which differences matter, and gets it wrong
/// for a patched build or a forgotten bump. The question being asked is whether
/// this daemon can answer for the policy, and its payload answers that
/// directly: the key is always present on a daemon that knows about it, empty
/// when there is none to state.
///
/// Verbs whose correctness depends on the policy call this and stop. Producing
/// a settings document that silently lacks a permission rule is exactly the
/// shape of failure this proxy exists to prevent — it would look like a
/// complete document and behave like an incomplete one.
pub fn require_client_policy(result: &serde_json::Value) -> Result<(), ProxyError> {
    if result.get("settings").is_some() {
        return Ok(());
    }

    Err(ProxyError::invalid_request(
        "The running daemon is from an older build and has no client policy to give. Restart \
         the daemon and try again.",
    ))
}

/// Serve the control socket until the process stops.
#[cfg(unix)]
pub async fn serve(path: &Path, state: ControlState) -> Result<(), ProxyError> {
    ensure_addressable(path)?;

    // A socket left behind by a crashed daemon would refuse the bind. Removing
    // it is safe here because the port bind has already established that no
    // other daemon is running.
    let _ = std::fs::remove_file(path);

    let listener = tokio::net::UnixListener::bind(path).map_err(|error| {
        ProxyError::invalid_request(format!("could not bind the control socket: {error}"))
    })?;

    // The socket carries no authentication, so the filesystem is the access
    // control: owner-only, like the credentials it can clear.
    restrict(path);

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _ = handle_connection(stream, state).await;
        });
    }
}

#[cfg(unix)]
fn restrict(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

/// One connection, one or more requests, each a line of JSON.
#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: ControlState,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let response = answer(&state, &line).await;
        let mut body = serde_json::to_string(&response).unwrap_or_default();
        body.push('\n');
        writer.write_all(body.as_bytes()).await?;
        writer.flush().await?;

        // Only now. A stop that released the run loop before this flush would
        // race the process against its own answer, and a caller reading a
        // closed connection cannot tell a clean stop from a crash.
        if state.shutdown.requested() {
            state.shutdown.release();
            break;
        }
    }

    Ok(())
}

/// Turn one request line into one response. Pure, so the whole vocabulary is
/// testable without a socket.
pub async fn answer(state: &ControlState, line: &str) -> Response {
    let request: Request = match serde_json::from_str(line) {
        Ok(request) => request,
        Err(error) => {
            return Response::failed(
                serde_json::Value::Null,
                codes::PARSE_ERROR,
                format!("malformed request: {error}"),
            );
        }
    };

    if request.jsonrpc != protocol::VERSION {
        return Response::failed(
            request.id,
            codes::INVALID_REQUEST,
            format!("unsupported jsonrpc version `{}`", request.jsonrpc),
        );
    }

    match handler::dispatch(state, &request.method, request.params.as_ref()).await {
        Ok(result) => Response::ok(request.id, result),
        Err(error) => {
            let code = if error.status == axum::http::StatusCode::NOT_FOUND {
                codes::METHOD_NOT_FOUND
            } else {
                codes::APPLICATION_ERROR
            };
            Response::failed(request.id, code, error.message)
        }
    }
}

/// Send one request to a running daemon and return its result.
#[cfg(unix)]
pub async fn call(
    path: &Path,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, ProxyError> {
    ensure_addressable(path)?;

    let stream = tokio::net::UnixStream::connect(path).await.map_err(|error| {
        ProxyError::invalid_request(format!(
            "could not reach the daemon at {}: {error}. Is it running? Start it with `proxenos run`.",
            path.display()
        ))
    })?;

    let (reader, mut writer) = stream.into_split();
    let mut body = serde_json::to_string(&Request::new(1, method, params)).unwrap_or_default();
    body.push('\n');
    writer
        .write_all(body.as_bytes())
        .await
        .map_err(|error| ProxyError::invalid_request(format!("could not send: {error}")))?;
    writer
        .flush()
        .await
        .map_err(|error| ProxyError::invalid_request(format!("could not send: {error}")))?;

    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|error| ProxyError::invalid_request(format!("could not read: {error}")))?
        .ok_or_else(|| ProxyError::invalid_request("the daemon closed the connection"))?;

    let response: Response = serde_json::from_str(&line).map_err(|error| {
        ProxyError::invalid_request(format!("unreadable response from the daemon: {error}"))
    })?;

    match (response.result, response.error) {
        (Some(result), _) => Ok(result),
        // The code survives the trip. Flattening every failure into one kind
        // reads as tidy and costs a caller the one distinction it acts on:
        // "this daemon does not have that method" is a different situation from
        // "that method refused what you asked", and only the first is answered
        // by replacing the daemon.
        (None, Some(error)) if error.code == codes::METHOD_NOT_FOUND => {
            Err(ProxyError::not_found(error.message))
        }
        (None, Some(error)) => Err(ProxyError::invalid_request(error.message)),
        (None, None) => Err(ProxyError::invalid_request(
            "the daemon returned neither a result nor an error",
        )),
    }
}

// Windows serves the same vocabulary over a named pipe. The protocol and the
// handler are shared; only the listener differs.
#[cfg(windows)]
pub async fn serve(path: &Path, state: ControlState) -> Result<(), ProxyError> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let name = pipe_name(path);
    loop {
        let server = ServerOptions::new().create(&name).map_err(|error| {
            ProxyError::invalid_request(format!("could not create the control pipe: {error}"))
        })?;
        server.connect().await.map_err(|error| {
            ProxyError::invalid_request(format!("control pipe failed: {error}"))
        })?;

        let state = state.clone();
        tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(server);
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let response = answer(&state, &line).await;
                let mut body = serde_json::to_string(&response).unwrap_or_default();
                body.push('\n');
                if writer.write_all(body.as_bytes()).await.is_err() {
                    break;
                }
                let _ = writer.flush().await;

                // Same order as the other listener: released only once the
                // answer is out. Without this a stop is answered and never
                // acted on, which is the worse of the two failures because the
                // caller was told it worked.
                if state.shutdown.requested() {
                    state.shutdown.release();
                    break;
                }
            }
        });
    }
}

#[cfg(windows)]
pub async fn call(
    path: &Path,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, ProxyError> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let client = ClientOptions::new()
        .open(pipe_name(path))
        .map_err(|error| {
            ProxyError::invalid_request(format!(
                "could not reach the daemon: {error}. Is it running? Start it with `proxenos run`."
            ))
        })?;

    let (reader, mut writer) = tokio::io::split(client);
    let mut body = serde_json::to_string(&Request::new(1, method, params)).unwrap_or_default();
    body.push('\n');
    writer
        .write_all(body.as_bytes())
        .await
        .map_err(|error| ProxyError::invalid_request(format!("could not send: {error}")))?;
    writer
        .flush()
        .await
        .map_err(|error| ProxyError::invalid_request(format!("could not send: {error}")))?;

    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|error| ProxyError::invalid_request(format!("could not read: {error}")))?
        .ok_or_else(|| ProxyError::invalid_request("the daemon closed the connection"))?;

    let response: Response = serde_json::from_str(&line).map_err(|error| {
        ProxyError::invalid_request(format!("unreadable response from the daemon: {error}"))
    })?;

    match (response.result, response.error) {
        (Some(result), _) => Ok(result),
        // The code survives the trip. Flattening every failure into one kind
        // reads as tidy and costs a caller the one distinction it acts on:
        // "this daemon does not have that method" is a different situation from
        // "that method refused what you asked", and only the first is answered
        // by replacing the daemon.
        (None, Some(error)) if error.code == codes::METHOD_NOT_FOUND => {
            Err(ProxyError::not_found(error.message))
        }
        (None, Some(error)) => Err(ProxyError::invalid_request(error.message)),
        (None, None) => Err(ProxyError::invalid_request(
            "the daemon returned neither a result nor an error",
        )),
    }
}

#[cfg(windows)]
fn pipe_name(path: &Path) -> String {
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("proxenos");
    format!(r"\\.\pipe\{stem}")
}
