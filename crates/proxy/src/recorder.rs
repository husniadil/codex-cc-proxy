//! Capturing exchanges as fixtures.
//!
//! Two modes, and the distinction matters because only one of them costs
//! anything. Ingress capture records what the client sends, before translation,
//! and needs no credentials at all. Upstream capture records what the backend
//! sends back, and spends quota.
//!
//! Empty-stream capture is neither: it is always on, because §5.4 says an empty
//! stream is always a defect and is otherwise invisible.

use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

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

        match serde_json::to_string_pretty(&capture) {
            Ok(body) => match std::fs::write(&path, body) {
                Ok(()) => {
                    tracing::info!(path = %path.display(), "recorded an exchange");
                    Some(path)
                }
                Err(error) => {
                    tracing::warn!(%error, "could not write the capture");
                    None
                }
            },
            Err(error) => {
                tracing::warn!(%error, "could not serialize the capture");
                None
            }
        }
    }
}
