//! Reading the two claims that matter.
//!
//! Nothing here verifies a signature, and nothing should. These tokens are not
//! being trusted — they were just received over TLS from the authorization
//! server that issued them, and the proxy is reading its own credentials to
//! learn when they expire and which account they belong to. Verification would
//! require the issuer's keys and would answer a question nobody is asking.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

/// The claim namespace the account id lives under.
const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// Decode a JWT's payload without verifying it.
fn payload(token: &str) -> Option<Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// The `exp` claim, as Unix seconds.
///
/// The token response carries no expiry field of its own, so this is the only
/// place the answer exists. Without it every request would look due for
/// refresh.
pub fn expiry(access_token: &str) -> Option<u64> {
    payload(access_token)?.get("exp")?.as_u64()
}

/// The account this grant belongs to, sent upstream as a header.
pub fn account_id(id_token: Option<&str>) -> Option<String> {
    let claims = payload(id_token?)?;
    claims
        .get(AUTH_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}
