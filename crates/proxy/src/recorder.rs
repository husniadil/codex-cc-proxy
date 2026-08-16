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

/// A captured exchange, in the corpus format.
#[derive(Debug, Serialize)]
struct Capture<'a> {
    name: String,
    capability: &'a str,
    provenance: &'a str,
    note: String,
    request: &'a Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    upstream: Vec<Value>,
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
    pub fn record(
        &self,
        mode: Mode,
        request: &Value,
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
