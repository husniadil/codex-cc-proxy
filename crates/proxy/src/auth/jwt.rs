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
    claim(id_token, "chatgpt_account_id")
}

/// The subscription this account holds, reported and never acted on.
///
/// Models and efforts are gated on the plan, and a refusal names the value it
/// rejected rather than the reason — `Invalid value: 'ultra'` reads as a broken
/// request when it is an unentitled one. Surfacing the plan is what lets a
/// caller tell those apart without guessing.
///
/// It is not used to decide anything. The plan the account had when it last
/// authenticated is not necessarily the plan it has now, and the backend is the
/// only authority on what it will accept.
pub fn plan(id_token: Option<&str>) -> Option<String> {
    claim(id_token, "chatgpt_plan_type")
}

/// Which account authenticated, for telling two grants apart.
///
/// A top-level claim rather than a namespaced one, and reported for one reason:
/// an operator with more than one subscription needs to know which of them this
/// daemon is spending. It goes nowhere near a request.
pub fn email(id_token: Option<&str>) -> Option<String> {
    payload(id_token?)?
        .get("email")?
        .as_str()
        .map(str::to_owned)
}

fn claim(id_token: Option<&str>, name: &str) -> Option<String> {
    payload(id_token?)?
        .get(AUTH_CLAIM)?
        .get(name)?
        .as_str()
        .map(str::to_owned)
}
