//! `docs/proxy-behavior.md` §8 — the authorization request.

use super::pkce::Pkce;
use super::pkce::random_state;

/// The authorization server.
pub const ISSUER: &str = "https://auth.openai.com";

/// The public client this flow authenticates as.
///
/// Public clients hold no secret, which is what PKCE exists to compensate for.
/// This proxy runs its own flow and owns its own refresh-token family — it does
/// not read or write credentials belonging to any other tool, because families
/// rotate and sharing one means whichever client refreshes last invalidates the
/// other.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// The scopes requested at login. Sent here and nowhere else: including
/// `scope` on a refresh re-scopes the grant and invalidates sibling
/// refresh-token families (§8).
///
/// Exactly what this proxy uses and nothing more. `openid`, `profile` and
/// `email` produce the id token the account id is read from; `offline_access`
/// produces the refresh token without which every session would need a fresh
/// login.
///
/// Connector scopes are deliberately absent. This proxy never invokes a
/// connector, and the authorization server refuses the whole request when a
/// client asks for a scope it is not allowed — so an unused scope is not merely
/// untidy, it is the difference between logging in and not.
pub const SCOPE: &str = "openid profile email offline_access";

/// The loopback port the authorization server will redirect to. Fixed, because
/// the redirect URI has to match one the server already accepts.
pub const CALLBACK_PORT: u16 = 1455;

pub const CALLBACK_PATH: &str = "/auth/callback";

/// One login attempt in progress.
#[derive(Debug)]
pub struct Authorization {
    pub url: String,
    pub state: String,
    pub pkce: Pkce,
    pub redirect_uri: String,
}

/// Build the URL the user opens to authorize.
pub fn begin(port: u16) -> Authorization {
    let pkce = Pkce::generate();
    let state = random_state();
    let redirect_uri = format!("http://localhost:{port}{CALLBACK_PATH}");

    let query = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", SCOPE),
        ("code_challenge", pkce.challenge()),
        ("code_challenge_method", pkce.method()),
        // Without this the id token carries no organization claims, and the
        // account id read from it is absent.
        ("id_token_add_organizations", "true"),
        ("state", state.as_str()),
    ];

    let encoded = query
        .iter()
        .map(|(key, value)| format!("{key}={}", encode(value)))
        .collect::<Vec<_>>()
        .join("&");

    Authorization {
        url: format!("{ISSUER}/oauth/authorize?{encoded}"),
        state,
        pkce,
        redirect_uri,
    }
}

/// The token endpoint, for both the code exchange and refresh.
pub fn token_endpoint() -> String {
    format!("{ISSUER}/oauth/token")
}

/// Percent-encode a query value.
///
/// Spaces become `%20`, not `+`. The scope string is the only value here that
/// contains one, and a `+` in a query value is not universally read as a space.
fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
