//! OS keychain lookups, behind the `keychain` feature.

use crate::secret::{Secret, SecretError};

/// Looks a secret up in the OS keychain.
///
/// # Errors
///
/// The keychain feature is not enabled, or the entry cannot be read.
pub fn lookup(service: &str) -> Result<Secret, SecretError> {
    Err(SecretError(format!(
        "the keychain secret `{service}` needs a build with the `keychain` feature"
    )))
}
