//! `docs/proxy-behavior.md` §4.4 — request compression.
//!
//! This module is the HTTP half: zstd on the body, announced with
//! `Content-Encoding`.
//!
//! The socket half is not here, because it is not a per-message decision.
//! WebSocket compression is `permessage-deflate`, negotiated once during the
//! upgrade and applied by the library to every frame — see
//! `upstream::websocket`. A binary frame is not an alternative way to say
//! "compressed": nothing in the protocol attaches that meaning to it, and the
//! backend simply fails to parse it.

use crate::error::ProxyError;

/// Compress a JSON body.
pub fn zstd(payload: &str) -> Result<Vec<u8>, ProxyError> {
    zstd::encode_all(payload.as_bytes(), 3)
        .map_err(|error| ProxyError::overloaded(format!("could not compress the request: {error}")))
}

/// Whether compressing this payload is worth doing.
///
/// Small payloads grow once the frame is counted. Sending those uncompressed is
/// the correct outcome, not an optimization that failed.
pub fn worth_compressing(payload: &str) -> bool {
    payload.len() > 1024
}
