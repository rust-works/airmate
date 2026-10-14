//! Thinking signatures.
//!
//! Anthropic signs thinking blocks to bind them to the account that produced them, and a client
//! sends them back on the next turn. No OpenAI-protocol upstream signs anything, so airmate signs
//! its own:
//!
//! ```text
//! am1.<base64url payload>.<base64url HMAC-SHA256(key, "am1" || u64_be(len(payload)) || payload || thinking)>
//! ```
//!
//! The key is derived from the gateway token, so signatures survive restarts and stop verifying
//! when the token is rotated. The payload is **signed, not encrypted**: a client can read the
//! reasoning it was told was omitted.

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::ZeroizeOnDrop;

use crate::ir::{Origin, Protocol};

const PREFIX: &str = "am1";
const KEY_CONTEXT: &[u8] = b"airmate thinking signature v1\0";
const PAYLOAD_VERSION: u8 = 1;

/// The HMAC key. Redacted in `Debug` and zeroed on drop.
#[derive(Clone, ZeroizeOnDrop)]
pub struct SigningKey([u8; 32]);

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

impl SigningKey {
    /// Derives the key from the gateway token: `SHA-256(context || token)`.
    #[must_use]
    pub fn from_token(token: &str) -> Self {
        let mut h = Sha256::new();
        h.update(KEY_CONTEXT);
        h.update(token.as_bytes());
        Self(h.finalize().into())
    }

    fn mac(&self, payload: &[u8], thinking: &str) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.0)
            .expect("HMAC accepts any key length");
        mac.update(PREFIX.as_bytes());
        mac.update(&(payload.len() as u64).to_be_bytes());
        mac.update(payload);
        mac.update(thinking.as_bytes());
        mac
    }
}

/// What a signature carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePayload {
    /// The upstream that produced the reasoning.
    pub origin: Origin,
    /// The Responses API's `encrypted_content`, which lets the next turn replay reasoning with
    /// `store: false`.
    pub encrypted_content: Option<Vec<u8>>,
    /// For `display: "omitted"`, the hidden reasoning text.
    pub hidden_text: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    v: u8,
    upstream: String,
    protocol: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hidden: Option<String>,
}

/// Why a signature did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// Not an `am1.` signature (for example one produced by Anthropic).
    Foreign,
    /// Looks like an `am1.` signature but is malformed.
    Malformed,
    /// The payload version is not understood.
    UnsupportedVersion,
    /// The MAC does not match: wrong token, or the thinking text was altered.
    Mismatch,
}

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Foreign => "not an airmate signature",
            Self::Malformed => "malformed airmate signature",
            Self::UnsupportedVersion => "unsupported airmate signature version",
            Self::Mismatch => "airmate signature does not verify",
        })
    }
}

impl std::error::Error for SignatureError {}

/// Signs `thinking` together with `payload`.
#[must_use]
pub fn sign(key: &SigningKey, payload: &SignaturePayload, thinking: &str) -> String {
    let wire = Wire {
        v: PAYLOAD_VERSION,
        upstream: payload.origin.upstream.clone(),
        protocol: payload.origin.protocol.as_str().to_owned(),
        enc: payload
            .encrypted_content
            .as_ref()
            .map(|b| URL_SAFE_NO_PAD.encode(b)),
        hidden: payload.hidden_text.clone(),
    };
    let bytes = serde_json::to_vec(&wire).expect("a flat struct of strings serialises");
    let tag = key.mac(&bytes, thinking).finalize().into_bytes();
    format!(
        "{PREFIX}.{}.{}",
        URL_SAFE_NO_PAD.encode(&bytes),
        URL_SAFE_NO_PAD.encode(tag)
    )
}

/// Verifies `signature` over `thinking` in constant time and returns its payload.
///
/// # Errors
///
/// See [`SignatureError`].
pub fn verify(
    key: &SigningKey,
    signature: &str,
    thinking: &str,
) -> Result<SignaturePayload, SignatureError> {
    let rest = signature
        .strip_prefix(PREFIX)
        .and_then(|r| r.strip_prefix('.'))
        .ok_or(SignatureError::Foreign)?;
    let (payload_b64, tag_b64) = rest.split_once('.').ok_or(SignatureError::Malformed)?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|_| SignatureError::Malformed)?;
    let tag = URL_SAFE_NO_PAD
        .decode(tag_b64)
        .map_err(|_| SignatureError::Malformed)?;
    key.mac(&payload, thinking)
        .verify_slice(&tag)
        .map_err(|_| SignatureError::Mismatch)?;

    let wire: Wire = serde_json::from_slice(&payload).map_err(|_| SignatureError::Malformed)?;
    if wire.v != PAYLOAD_VERSION {
        return Err(SignatureError::UnsupportedVersion);
    }
    let protocol = Protocol::parse(&wire.protocol).ok_or(SignatureError::Malformed)?;
    let encrypted_content = match wire.enc {
        Some(e) => Some(
            URL_SAFE_NO_PAD
                .decode(e)
                .map_err(|_| SignatureError::Malformed)?,
        ),
        None => None,
    };
    Ok(SignaturePayload {
        origin: Origin {
            upstream: wire.upstream,
            protocol,
        },
        encrypted_content,
        hidden_text: wire.hidden,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> SignaturePayload {
        SignaturePayload {
            origin: Origin {
                upstream: "openai".into(),
                protocol: Protocol::Responses,
            },
            encrypted_content: Some(vec![0, 1, 2, 255]),
            hidden_text: Some("secret thoughts".into()),
        }
    }

    #[test]
    fn round_trips_payload_and_thinking() {
        let key = SigningKey::from_token("token");
        let sig = sign(&key, &payload(), "I think");
        assert!(sig.starts_with("am1."));
        assert_eq!(verify(&key, &sig, "I think"), Ok(payload()));
    }

    #[test]
    fn round_trips_a_minimal_payload() {
        let key = SigningKey::from_token("token");
        let p = SignaturePayload {
            origin: Origin {
                upstream: "local".into(),
                protocol: Protocol::Chat,
            },
            encrypted_content: None,
            hidden_text: None,
        };
        assert_eq!(verify(&key, &sign(&key, &p, ""), ""), Ok(p));
    }

    #[test]
    fn altered_thinking_does_not_verify() {
        let key = SigningKey::from_token("token");
        let sig = sign(&key, &payload(), "I think");
        assert_eq!(
            verify(&key, &sig, "I think!"),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn a_rotated_token_does_not_verify() {
        let sig = sign(&SigningKey::from_token("old"), &payload(), "x");
        assert_eq!(
            verify(&SigningKey::from_token("new"), &sig, "x"),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn a_tampered_payload_does_not_verify() {
        let key = SigningKey::from_token("token");
        let sig = sign(&key, &payload(), "x");
        let mut parts: Vec<&str> = sig.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(br#"{"v":1,"upstream":"evil","protocol":"chat"}"#);
        parts[1] = &forged;
        assert_eq!(
            verify(&key, &parts.join("."), "x"),
            Err(SignatureError::Mismatch)
        );
    }

    #[test]
    fn the_payload_length_is_bound_into_the_mac() {
        // Moving bytes between payload and thinking must not keep a signature valid.
        let key = SigningKey::from_token("token");
        let sig = sign(&key, &payload(), "abc");
        assert_eq!(verify(&key, &sig, "ab"), Err(SignatureError::Mismatch));
    }

    #[test]
    fn foreign_and_malformed_signatures_are_told_apart() {
        let key = SigningKey::from_token("token");
        assert_eq!(
            verify(&key, "EqQBCkYIAxgC", "x"),
            Err(SignatureError::Foreign)
        );
        assert_eq!(
            verify(&key, "am1.onlyone", "x"),
            Err(SignatureError::Malformed)
        );
        assert_eq!(
            verify(&key, "am1.!!!.!!!", "x"),
            Err(SignatureError::Malformed)
        );
        assert_eq!(verify(&key, "", "x"), Err(SignatureError::Foreign));
    }

    #[test]
    fn a_future_version_is_reported() {
        let key = SigningKey::from_token("token");
        let bytes = br#"{"v":2,"upstream":"u","protocol":"chat"}"#;
        let tag = key.mac(bytes, "x").finalize().into_bytes();
        let sig = format!(
            "am1.{}.{}",
            URL_SAFE_NO_PAD.encode(bytes),
            URL_SAFE_NO_PAD.encode(tag)
        );
        assert_eq!(
            verify(&key, &sig, "x"),
            Err(SignatureError::UnsupportedVersion)
        );
    }

    #[test]
    fn a_validly_signed_but_unparseable_payload_is_malformed() {
        let key = SigningKey::from_token("token");
        for bytes in [
            &b"not json"[..],
            br#"{"v":1,"upstream":"u","protocol":"smoke"}"#,
            br#"{"v":1,"upstream":"u","protocol":"chat","enc":"***"}"#,
        ] {
            let tag = key.mac(bytes, "x").finalize().into_bytes();
            let sig = format!(
                "am1.{}.{}",
                URL_SAFE_NO_PAD.encode(bytes),
                URL_SAFE_NO_PAD.encode(tag)
            );
            assert_eq!(verify(&key, &sig, "x"), Err(SignatureError::Malformed));
        }
    }

    #[test]
    fn signing_is_deterministic() {
        let key = SigningKey::from_token("token");
        assert_eq!(sign(&key, &payload(), "x"), sign(&key, &payload(), "x"));
    }

    #[test]
    fn the_key_is_redacted_in_debug_output() {
        let shown = format!("{:?}", SigningKey::from_token("token"));
        assert_eq!(shown, "SigningKey(<redacted>)");
        assert!(!SignatureError::Mismatch.to_string().is_empty());
        assert!(!SignatureError::Foreign.to_string().is_empty());
        assert!(!SignatureError::Malformed.to_string().is_empty());
        assert!(!SignatureError::UnsupportedVersion.to_string().is_empty());
    }
}
