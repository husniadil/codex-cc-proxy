//! PKCE, as `docs/proxy-behavior.md` §8 requires.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use sha2::Digest;
use sha2::Sha256;

/// A verifier and the challenge derived from it.
#[derive(Clone)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The verifier is a secret for the length of the flow: anyone holding
        // it plus the authorization code can complete the exchange.
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

impl Pkce {
    /// Generate a fresh pair.
    pub fn generate() -> Self {
        // 64 bytes of randomness, encoded. The specification permits 43 to 128
        // characters; this sits comfortably inside that and leaves no question
        // about entropy.
        let mut bytes = [0u8; 64];
        rand::rng().fill(&mut bytes);
        Self::from_verifier(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn from_verifier(verifier: impl Into<String>) -> Self {
        let verifier = verifier.into();
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = URL_SAFE_NO_PAD.encode(digest);
        Self {
            verifier,
            challenge,
        }
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// Always S256. The `plain` method offers no protection at all, and an
    /// authorization server that accepts it will accept S256 too.
    pub fn method(&self) -> &'static str {
        "S256"
    }
}

/// An opaque value tying an authorization response back to the request that
/// began it.
pub fn random_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
