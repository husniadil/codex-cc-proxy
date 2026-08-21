//! Capturing exchanges as fixtures.
//!
//! Two modes, and the distinction matters because only one of them costs
//! anything. Ingress capture records what the client sends, before translation,
//! and needs no credentials at all. Upstream capture records what the backend
//! sends back, and spends quota.
//!
//! Empty-stream capture is neither: it is always on, because §5.4 says an empty
//! stream is always a defect and is otherwise invisible.
//!
//! **A capture is conversation content.** It holds the system prompt, the
//! messages, and whatever the tools read — file contents included. It is not a
//! credential, but it is not public either, so captures are written to the
//! per-user configuration directory rather than a shared temporary one, created
//! `0600`, and bounded so a repeating failure cannot fill a disk.

use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// How many captures to keep. Enough to see a pattern, few enough that a
/// failure loop cannot run a disk out of space.
const KEEP: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// What the client sends. Free.
    Ingress,
    /// What the backend returns. Spends quota.
    Upstream,
}

/// Which captures are switched on, shared between the ingress path and the
/// control socket.
///
/// Shared rather than copied, because `record.start` is meant to change what a
/// *running* daemon captures. A copy taken at startup makes the method report
/// success and do nothing — a plausible answer to a request that had no effect,
/// which is the one failure this project refuses everywhere else.
#[derive(Debug, Default)]
pub struct Switches {
    ingress: std::sync::atomic::AtomicBool,
    upstream: std::sync::atomic::AtomicBool,
}

impl Switches {
    pub fn new(mode: Option<Mode>) -> Self {
        let switches = Self::default();
        if let Some(mode) = mode {
            switches.start(mode);
        }
        switches
    }

    pub fn ingress(&self) -> bool {
        self.ingress.load(Ordering::SeqCst)
    }

    pub fn upstream(&self) -> bool {
        self.upstream.load(Ordering::SeqCst)
    }

    /// Whether anything beyond the always-on empty-stream capture is running.
    pub fn any(&self) -> bool {
        self.ingress() || self.upstream()
    }

    /// Turn one mode on. The other is untouched: they cost differently, and
    /// starting an upstream capture by asking for an ingress one would spend
    /// quota nobody asked to spend.
    pub fn start(&self, mode: Mode) {
        match mode {
            Mode::Ingress => self.ingress.store(true, Ordering::SeqCst),
            Mode::Upstream => self.upstream.store(true, Ordering::SeqCst),
        }
    }

    pub fn stop(&self) {
        self.ingress.store(false, Ordering::SeqCst);
        self.upstream.store(false, Ordering::SeqCst);
    }
}

/// A captured exchange, in the corpus format.
///
/// The request is generic over how it is held, because the two paths hold it
/// differently and one of them must not be re-encoded. The translating path
/// captures the request it parsed; the relay (§9) captures the bytes it
/// relayed, so what is written is what the backend was sent rather than what
/// this proxy's own types would reproduce from it.
#[derive(Debug, Serialize)]
struct Capture<'a, R: ?Sized> {
    name: String,
    capability: &'a str,
    provenance: &'a str,
    note: String,
    request: &'a R,
    /// The request headers as they arrived, in arrival order, repeats kept —
    /// both are data when the question is what a client actually sends.
    /// Credential-bearing values are redacted before they reach this struct.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    headers: Vec<(String, String)>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    upstream: Vec<Value>,
}

/// Header values that are credentials, matched by name. Redacted at the edge
/// rather than trusted to be harmless: a capture is written to disk, and a
/// secret in a file that is not the credential store is a leak however it got
/// there. The name survives — that the client sent it is the datum.
const CREDENTIAL_HEADERS: [&str; 4] = [
    "authorization",
    "x-api-key",
    "cookie",
    "proxy-authorization",
];

/// The recordable form of a header set: names lowercased by the HTTP layer,
/// credential values replaced, a non-UTF-8 value named as such rather than
/// mangled.
pub fn presentable_headers(headers: &axum::http::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = name.as_str().to_owned();
            let value = if CREDENTIAL_HEADERS.contains(&name.as_str()) {
                "(redacted)".to_owned()
            } else {
                value
                    .to_str()
                    .unwrap_or("(a value that is not valid UTF-8)")
                    .to_owned()
            };
            (name, value)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct Recorder {
    directory: PathBuf,
    sequence: Arc<AtomicU64>,
}

impl Recorder {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Where captures go by default: beside the configuration, not in a shared
    /// temporary directory every local process can read.
    pub fn default_directory() -> PathBuf {
        crate::config::config_dir().join("captures")
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Record one exchange. Returns the file written.
    ///
    /// Recording never fails a request: a capture that cannot be written is
    /// logged and dropped. Losing a fixture is a nuisance; losing the turn it
    /// was recording is a broken session.
    pub fn record<R: Serialize + std::fmt::Debug + ?Sized>(
        &self,
        mode: Mode,
        request: &R,
        headers: Vec<(String, String)>,
        upstream: Vec<Value>,
        note: &str,
    ) -> Option<PathBuf> {
        let index = self.sequence.fetch_add(1, Ordering::Relaxed);
        let prefix = match mode {
            Mode::Ingress => "ingress",
            Mode::Upstream => "upstream",
        };
        let name = format!("{prefix}-{index:04}");

        let capture = Capture {
            name: name.clone(),
            // A capture cannot know which capability it exercises. Whoever
            // promotes it into the corpus decides that, and the placeholder is
            // there to be replaced rather than to be believed.
            capability: "tool-calling",
            provenance: "captured",
            note: note.to_owned(),
            request,
            headers,
            upstream,
        };

        let path = self.directory.join(format!("{name}.json"));
        if let Err(error) = std::fs::create_dir_all(&self.directory) {
            tracing::warn!(%error, "could not create the capture directory");
            return None;
        }
        restrict_directory(&self.directory);

        let body = match serde_json::to_string_pretty(&capture) {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(%error, "could not serialize the capture");
                return None;
            }
        };

        if let Err(error) = write_private(&path, &body) {
            tracing::warn!(%error, "could not write the capture");
            return None;
        }

        self.prune();
        tracing::info!(path = %path.display(), "recorded an exchange");
        Some(path)
    }

    /// Keep the most recent captures and remove the rest.
    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };

        let mut captures: Vec<(std::time::SystemTime, PathBuf)> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .filter_map(|path| {
                let modified = path.metadata().ok()?.modified().ok()?;
                Some((modified, path))
            })
            .collect();

        if captures.len() <= KEEP {
            return;
        }

        // Oldest first, so the tail is what goes.
        captures.sort_by_key(|(modified, _)| *modified);
        let excess = captures.len().saturating_sub(KEEP);
        for (_, path) in captures.into_iter().take(excess) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) {}

/// Created `0600` from the outset, like the credentials beside it. Writing
/// first and tightening afterwards leaves a window in which the file is
/// world-readable.
#[cfg(unix)]
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(body.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    std::fs::write(path, body)
}
