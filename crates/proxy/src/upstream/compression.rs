//! `docs/proxy-behavior.md` §4.4 — request compression.
//!
//! zstd applies to the HTTP body and is announced with `Content-Encoding`.
//! There is no way to announce it on a WebSocket message: the payload is JSON
//! text either way, and a binary frame carries no signal that says otherwise —
//! the backend simply fails to parse it and refuses the request. WebSocket
//! compression is `permessage-deflate`, negotiated in the upgrade, which this
//! client cannot yet offer.

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
