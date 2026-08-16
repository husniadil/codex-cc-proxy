//! `docs/proxy-behavior.md` §8 — where credentials live.
//!
//! Behind a trait, so a platform keychain satisfies the same contract as the
//! default file. Credentials never appear in process arguments, logs, or the
//! configuration file.

use crate::error::ProxyError;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

/// One grant. `Debug` is implemented by hand: the derived one would print the
/// tokens, and a `Debug` line in a log is exactly the leak §8 forbids.
#[derive(Clone, Deserialize, Serialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Unix seconds. Absolute rather than a duration, because a duration is
    /// only meaningful next to the instant it was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &Redacted)
            .field("refresh_token", &Redacted)
            .field("id_token", &self.id_token.as_ref().map(|_| Redacted))
            .field("account_id", &self.account_id)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

struct Redacted;

impl std::fmt::Debug for Redacted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Credentials {
    /// Whether the access token is at or past the point where it should be
    /// replaced.
    ///
    /// Refresh begins ahead of expiry (§8): a token that expires during a
    /// request fails the request, and the margin is what stops that being
    /// routine.
    pub fn needs_refresh(&self, now: u64, margin_seconds: u64) -> bool {
        match self.expires_at {
            Some(expires_at) => now.saturating_add(margin_seconds) >= expires_at,
            // An unknown expiry is treated as expired. Refreshing needlessly
            // costs one request; using a dead token fails the turn.
            None => true,
        }
    }
}

pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Credentials>, ProxyError>;
    fn save(&self, credentials: &Credentials) -> Result<(), ProxyError>;
    fn clear(&self) -> Result<(), ProxyError>;
}

/// The default implementation: one JSON file, created `0600`.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl CredentialStore for FileStore {
    fn load(&self) -> Result<Option<Credentials>, ProxyError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(ProxyError::authentication(format!(
                    "could not read credentials: {error}"
                )));
            }
        };

        serde_json::from_str(&raw).map(Some).map_err(|error| {
            // The error names the parse failure, never the content.
            ProxyError::authentication(format!("stored credentials are unreadable: {error}"))
        })
    }

    fn save(&self, credentials: &Credentials) -> Result<(), ProxyError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ProxyError::authentication(format!(
                    "could not create credential directory: {error}"
                ))
            })?;
        }

        let body = serde_json::to_string_pretty(credentials).map_err(|error| {
            ProxyError::authentication(format!("could not serialize credentials: {error}"))
        })?;

        // Created with restrictive permissions from the outset. Writing first
        // and tightening afterwards leaves a window in which the file is
        // world-readable, and that window is enough.
        write_private(&self.path, &body)
    }

    fn clear(&self) -> Result<(), ProxyError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(ProxyError::authentication(format!(
                "could not clear credentials: {error}"
            ))),
        }
    }
}

#[cfg(unix)]
fn write_private(path: &Path, body: &str) -> Result<(), ProxyError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            ProxyError::authentication(format!("could not open credential file: {error}"))
        })?;

    file.write_all(body.as_bytes()).map_err(|error| {
        ProxyError::authentication(format!("could not write credentials: {error}"))
    })
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) -> Result<(), ProxyError> {
    // Windows has no mode bits. The file inherits the directory's ACL, and the
    // configuration directory is per-user.
    std::fs::write(path, body).map_err(|error| {
        ProxyError::authentication(format!("could not write credentials: {error}"))
    })
}
