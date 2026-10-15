//! Inbound authentication: the gateway token and named client keys.
//!
//! A token is 32 bytes from the OS CSPRNG, base64url, accepted as `x-api-key` or
//! `Authorization: Bearer`. The default single token is created on first use and kept in the state
//! directory (`0600`, directory `0700`) so client settings survive restarts. Named client keys are
//! stored as SHA-256 hashes only. Comparison is constant time. A token is never accepted as a
//! command-line flag: it would land in `ps` and shell history.

use std::fmt;
use std::io::Write;
use std::path::Path;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::config::schema::ClientConfig;
use crate::secret::{check_owner_only, Secret};

/// The name given to requests authenticated by the gateway token.
pub const DEFAULT_CLIENT: &str = "default";

/// Why a token could not be loaded or created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthError(pub String);

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AuthError {}

fn err(message: impl Into<String>) -> AuthError {
    AuthError(message.into())
}

fn random_b64(bytes: usize) -> Result<String, AuthError> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| err(format!("the OS random source failed: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// A fresh gateway token: 32 random bytes, base64url.
///
/// # Errors
///
/// The OS random source failed.
pub fn generate_token() -> Result<String, AuthError> {
    random_b64(32)
}

/// A fresh client key: `am_` followed by 32 random bytes, base64url. Shown once.
///
/// # Errors
///
/// The OS random source failed.
pub fn generate_client_key() -> Result<String, AuthError> {
    Ok(format!("am_{}", random_b64(32)?))
}

/// `sha256:<hex>` of a client key, as stored in the configuration.
#[must_use]
pub fn hash_key(key: &str) -> String {
    format!("sha256:{}", hex(&Sha256::digest(key.as_bytes())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
    }
    Some(out)
}

/// Compares two 32-byte digests without an early exit.
fn digests_equal(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Constant-time equality of two tokens (by comparing their digests).
#[must_use]
pub fn token_eq(expected: &str, given: &str) -> bool {
    let a: [u8; 32] = Sha256::digest(expected.as_bytes()).into();
    let b: [u8; 32] = Sha256::digest(given.as_bytes()).into();
    digests_equal(&a, &b)
}

/// Loads the gateway token from `path`, creating it on first use.
///
/// The directory is created `0700` and the file `0600`. A token file that group or others can
/// read is refused.
///
/// # Errors
///
/// The file cannot be created or read, is too permissive, or is empty.
pub fn load_or_create_token(path: &Path) -> Result<Secret, AuthError> {
    if path.exists() {
        return read_token(path);
    }
    if let Some(dir) = path.parent() {
        let existed = dir.exists();
        std::fs::create_dir_all(dir)
            .map_err(|e| err(format!("cannot create {}: {e}", dir.display())))?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| err(format!("cannot restrict {}: {e}", dir.display())))?;
        }
        let _ = existed;
    }
    let token = generate_token()?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(mut f) => {
            f.write_all(token.as_bytes())
                .map_err(|e| err(format!("cannot write {}: {e}", path.display())))?;
            Ok(Secret::new(token))
        }
        // Lost a race with another process starting at the same moment: use its token.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => read_token(path),
        Err(e) => Err(err(format!("cannot create {}: {e}", path.display()))),
    }
}

fn read_token(path: &Path) -> Result<Secret, AuthError> {
    check_owner_only(path).map_err(|e| err(e.to_string()))?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| err(format!("cannot read {}: {e}", path.display())))?;
    let token = text.trim();
    if token.is_empty() {
        return Err(err(format!(
            "{} is empty; delete it to have airmate create a new token",
            path.display()
        )));
    }
    Ok(Secret::new(token))
}

/// Who a request is from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The client name.
    pub client: String,
}

/// Authenticates a presented token. An extension trait of the library API.
pub trait Authenticator: Send + Sync {
    /// The identity behind `presented`, or `None` if it is not valid.
    fn authenticate(&self, presented: &str) -> Option<Identity>;
}

/// The built-in authenticator: the gateway token plus named client keys.
pub struct TokenAuthenticator {
    gateway: [u8; 32],
    clients: Vec<(String, [u8; 32])>,
}

impl fmt::Debug for TokenAuthenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenAuthenticator")
            .field("clients", &self.clients.len())
            .finish_non_exhaustive()
    }
}

impl TokenAuthenticator {
    /// An authenticator for `gateway` and the configured `clients`.
    ///
    /// # Errors
    ///
    /// A client whose `key_hash` is malformed.
    pub fn new(gateway: &Secret, clients: &[ClientConfig]) -> Result<Self, AuthError> {
        let mut table = Vec::new();
        for c in clients {
            let hash = c
                .key_hash
                .strip_prefix("sha256:")
                .and_then(unhex)
                .ok_or_else(|| err(format!("client `{}` has a malformed key_hash", c.name)))?;
            table.push((c.name.clone(), hash));
        }
        Ok(Self {
            gateway: Sha256::digest(gateway.expose().as_bytes()).into(),
            clients: table,
        })
    }
}

impl Authenticator for TokenAuthenticator {
    fn authenticate(&self, presented: &str) -> Option<Identity> {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        // Check everything, so the time taken does not say which entry matched.
        let mut found: Option<&str> =
            digests_equal(&digest, &self.gateway).then_some(DEFAULT_CLIENT);
        for (name, hash) in &self.clients {
            if digests_equal(&digest, hash) && found.is_none() {
                found = Some(name);
            }
        }
        found.map(|client| Identity {
            client: client.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("airmate-auth-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn client(name: &str, key: &str) -> ClientConfig {
        ClientConfig {
            name: name.into(),
            key_hash: hash_key(key),
            models: vec![],
            rate: None,
            budget: None,
            budget_fallback: None,
        }
    }

    #[test]
    fn generated_tokens_are_43_url_safe_characters_and_unique() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 43);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
        let k = generate_client_key().unwrap();
        assert!(k.starts_with("am_") && k.len() == 46);
    }

    #[test]
    fn key_hashes_are_sha256_hex() {
        let h = hash_key("abc");
        assert_eq!(
            h,
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(unhex(&h[7..]).unwrap()[0], 0xba);
        assert_eq!(unhex("zz"), None);
        assert_eq!(unhex(&"g".repeat(64)), None);
        assert_eq!(unhex(&"é".repeat(32)), None);
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(token_eq("secret", "secret"));
        assert!(!token_eq("secret", "secre"));
        assert!(!token_eq("secret", "Secret"));
        assert!(!token_eq("secret", ""));
        assert!(token_eq("", ""));
    }

    #[test]
    fn the_token_is_created_once_and_then_reused() {
        let d = tmp("create");
        let p = d.join("state").join("gateway.token");
        let first = load_or_create_token(&p).unwrap();
        assert_eq!(first.expose().len(), 43);
        let again = load_or_create_token(&p).unwrap();
        assert_eq!(
            first.expose(),
            again.expose(),
            "client settings survive restarts"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), first.expose());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_and_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("perms");
        let p = d.join("state").join("gateway.token");
        load_or_create_token(&p).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_group_or_world_readable_token_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("loose");
        let p = d.join("gateway.token");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(&p, "tok").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o640)).unwrap();
        let e = load_or_create_token(&p).unwrap_err().to_string();
        assert!(e.contains("readable by group or others"), "{e}");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load_or_create_token(&p).unwrap().expose(), "tok");
        std::fs::write(&p, " \n").unwrap();
        assert!(load_or_create_token(&p)
            .unwrap_err()
            .to_string()
            .contains("empty"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn the_gateway_token_and_client_keys_authenticate() {
        let gw = Secret::new("gateway-token");
        let auth = TokenAuthenticator::new(
            &gw,
            &[client("ci", "ci-key"), client("laptop", "laptop-key")],
        )
        .unwrap();
        assert_eq!(
            auth.authenticate("gateway-token"),
            Some(Identity {
                client: "default".into()
            })
        );
        assert_eq!(
            auth.authenticate("ci-key"),
            Some(Identity {
                client: "ci".into()
            })
        );
        assert_eq!(auth.authenticate("laptop-key").unwrap().client, "laptop");
        for wrong in ["", "gateway-token ", "CI-KEY", "sha256:abc", "ci-ke"] {
            assert_eq!(auth.authenticate(wrong), None, "{wrong:?}");
        }
    }

    #[test]
    fn a_rotated_gateway_token_stops_working() {
        let old = TokenAuthenticator::new(&Secret::new("old"), &[]).unwrap();
        let new = TokenAuthenticator::new(&Secret::new("new"), &[]).unwrap();
        assert!(old.authenticate("old").is_some() && new.authenticate("old").is_none());
    }

    #[test]
    fn a_malformed_key_hash_is_rejected_when_building() {
        let mut bad = client("ci", "k");
        bad.key_hash = "sha256:nothex".into();
        let e = TokenAuthenticator::new(&Secret::new("g"), &[bad])
            .unwrap_err()
            .to_string();
        assert!(e.contains("`ci`") && e.contains("key_hash"), "{e}");
    }

    #[test]
    fn the_authenticator_never_prints_keys() {
        let auth =
            TokenAuthenticator::new(&Secret::new("gateway-token"), &[client("ci", "ci-key")])
                .unwrap();
        let shown = format!("{auth:?}");
        assert!(
            !shown.contains("gateway-token")
                && !shown.contains("ci-key")
                && shown.contains("clients")
        );
        assert!(!AuthError("x".into()).to_string().is_empty());
    }
}
