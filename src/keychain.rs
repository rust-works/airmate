//! OS keychain lookups, by asking the platform's own command-line tool, so airmate links no
//! keychain library and holds no keychain access of its own beyond what the tool is granted.
//!
//! - macOS: `security find-generic-password -s SERVICE -w`
//! - Linux and other Unix: `secret-tool lookup service SERVICE` (libsecret)
//! - Windows: not supported; use `env`, `file` or `command` secrets.
//!
//! Store an entry with `security add-generic-password -s SERVICE -a "$USER" -w` on macOS, or
//! `secret-tool store --label=SERVICE service SERVICE` on Linux.

use crate::config::paths::Platform;
use crate::secret::SecretError;

/// The command that prints the secret stored under `service` on `platform`.
///
/// # Errors
///
/// The platform has no supported keychain tool, or `service` is empty or looks like an option.
pub fn argv(service: &str, platform: Platform) -> Result<Vec<String>, SecretError> {
    if service.is_empty() || service.starts_with('-') {
        return Err(SecretError(format!(
            "`{service}` is not a usable keychain service name"
        )));
    }
    let v = |parts: &[&str]| parts.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    match platform {
        Platform::MacOs => {
            let mut a = v(&["security", "find-generic-password", "-s"]);
            a.push(service.to_owned());
            a.push("-w".to_owned());
            Ok(a)
        }
        Platform::Unix => {
            let mut a = v(&["secret-tool", "lookup", "service"]);
            a.push(service.to_owned());
            Ok(a)
        }
        Platform::Windows => Err(SecretError(
            "keychain secrets are not supported on Windows; use an env, file or command secret"
                .to_owned(),
        )),
    }
}

/// A hint about what to install or store when a lookup fails.
#[must_use]
pub fn hint(service: &str, platform: Platform) -> String {
    match platform {
        Platform::MacOs => {
            format!("store it with: security add-generic-password -s {service} -a \"$USER\" -w")
        }
        Platform::Unix => format!(
            "install libsecret's `secret-tool` and store it with: secret-tool store --label={service} service {service}"
        ),
        Platform::Windows => "use an env, file or command secret instead".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_platform_asks_its_own_tool() {
        assert_eq!(
            argv("airmate-fireworks", Platform::MacOs).unwrap(),
            [
                "security",
                "find-generic-password",
                "-s",
                "airmate-fireworks",
                "-w"
            ]
        );
        assert_eq!(
            argv("airmate-fireworks", Platform::Unix).unwrap(),
            ["secret-tool", "lookup", "service", "airmate-fireworks"]
        );
        assert!(argv("x", Platform::Windows)
            .unwrap_err()
            .0
            .contains("not supported on Windows"));
    }

    #[test]
    fn a_service_name_cannot_smuggle_an_option() {
        for bad in ["", "-w", "--help"] {
            assert!(argv(bad, Platform::MacOs).is_err(), "{bad:?}");
            assert!(argv(bad, Platform::Unix).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_hint_says_how_to_store_the_entry() {
        assert!(hint("svc", Platform::MacOs).contains("security add-generic-password -s svc"));
        assert!(hint("svc", Platform::Unix).contains("secret-tool store"));
    }
}
