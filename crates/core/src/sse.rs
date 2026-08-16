//! `docs/proxy-behavior.md` §5.0 — SSE framing.
//!
//! Bytes arrive from a socket in chunks that align with nothing: an event may
//! be split mid-line, mid-JSON, or mid-character. The decoder buffers until a
//! blank line terminates an event, then yields that event's `data` payload.

/// Accumulates bytes and yields complete event payloads.
#[derive(Debug, Default)]
pub struct SseDecoder {
    /// Bytes not yet forming a complete line.
    buffer: Vec<u8>,
    /// `data` lines of the event being assembled.
    data: Vec<String>,
    /// Whether the current event has carried a `data` field at all. An event
    /// with an empty `data:` line is not the same as one with none.
    has_data: bool,
    /// Set when the previous chunk ended on `\r`, so a `\n` opening the next
    /// chunk is the same terminator rather than an empty line.
    pending_newline_after_cr: bool,
}

impl SseDecoder {
    /// Consume a chunk, yielding every payload it completes.
    pub fn push(&mut self, chunk: &[u8]) -> impl Iterator<Item = String> + use<> {
        let mut payloads = Vec::new();

        for &byte in chunk {
            if self.pending_newline_after_cr {
                self.pending_newline_after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }

            match byte {
                b'\n' => self.end_line(&mut payloads),
                b'\r' => {
                    self.pending_newline_after_cr = true;
                    self.end_line(&mut payloads);
                }
                _ => self.buffer.push(byte),
            }
        }

        payloads.into_iter()
    }

    /// Flush an event left unterminated by the end of the stream.
    ///
    /// A stream that ends without its final blank line has still delivered that
    /// event, and discarding it loses the last thing the model said.
    pub fn finish(&mut self) -> Vec<String> {
        let mut payloads = Vec::new();
        if !self.buffer.is_empty() {
            self.end_line(&mut payloads);
        }
        self.dispatch(&mut payloads);
        payloads
    }

    fn end_line(&mut self, payloads: &mut Vec<String>) {
        let line = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();

        if line.is_empty() {
            self.dispatch(payloads);
            return;
        }

        // A line opening with a colon is a comment, sent by some servers as a
        // keep-alive. Treating one as an event emits a spurious frame.
        if line.starts_with(':') {
            return;
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            // A line with no colon is a field with an empty value.
            None => (line.as_str(), ""),
        };

        if field == "data" {
            self.has_data = true;
            self.data.push(value.to_owned());
        }
    }

    /// A blank line ends an event.
    fn dispatch(&mut self, payloads: &mut Vec<String>) {
        if !self.has_data {
            return;
        }
        self.has_data = false;
        payloads.push(std::mem::take(&mut self.data).join("\n"));
    }
}
