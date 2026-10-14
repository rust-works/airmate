//! Where airmate keeps its files.
//!
//! Resolution is implemented in-tree (no `dirs` dependency) and takes the environment as a
//! function so it can be tested. Directories are created `0700` and files `0600` by their owners.

use std::path::PathBuf;

/// Looks up an environment variable.
pub type EnvFn<'a> = &'a (dyn Fn(&str) -> Option<String> + Send + Sync);

/// The platform whose conventions apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Linux and other Unix systems.
    Unix,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
}

impl Platform {
    /// The platform this binary was built for.
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// The directories airmate uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// The config directory.
    pub config: PathBuf,
    /// The state directory: token, request log, usage ledger.
    pub state: PathBuf,
    /// The runtime directory: the admin socket.
    pub runtime: PathBuf,
}

fn non_empty(env: EnvFn<'_>, name: &str) -> Option<PathBuf> {
    env(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

impl Dirs {
    /// Resolves the directories for `platform` from `env`.
    #[must_use]
    pub fn resolve(platform: Platform, env: EnvFn<'_>) -> Self {
        let home = non_empty(env, "HOME")
            .or_else(|| non_empty(env, "USERPROFILE"))
            .unwrap_or_else(|| PathBuf::from("."));
        let (config, state) = match platform {
            Platform::Unix => (
                non_empty(env, "XDG_CONFIG_HOME")
                    .unwrap_or_else(|| home.join(".config"))
                    .join("airmate"),
                non_empty(env, "XDG_STATE_HOME")
                    .unwrap_or_else(|| home.join(".local/state"))
                    .join("airmate"),
            ),
            Platform::MacOs => {
                let base = home.join("Library/Application Support/airmate");
                (
                    non_empty(env, "XDG_CONFIG_HOME")
                        .map_or_else(|| base.clone(), |p| p.join("airmate")),
                    non_empty(env, "XDG_STATE_HOME")
                        .map_or_else(|| base.join("state"), |p| p.join("airmate")),
                )
            }
            Platform::Windows => {
                let roaming =
                    non_empty(env, "APPDATA").unwrap_or_else(|| home.join("AppData/Roaming"));
                let local =
                    non_empty(env, "LOCALAPPDATA").unwrap_or_else(|| home.join("AppData/Local"));
                (roaming.join("airmate"), local.join("airmate"))
            }
        };
        let runtime = non_empty(env, "XDG_RUNTIME_DIR")
            .map(|p| p.join("airmate"))
            .unwrap_or_else(|| state.join("run"));
        Self {
            config,
            state,
            runtime,
        }
    }

    /// The default config file path.
    #[must_use]
    pub fn default_config_file(&self) -> PathBuf {
        self.config.join("airmate.yaml")
    }

    /// The gateway token file.
    #[must_use]
    pub fn token_file(&self) -> PathBuf {
        self.state.join("gateway.token")
    }

    /// The request log.
    #[must_use]
    pub fn request_log(&self) -> PathBuf {
        self.state.join("requests.jsonl")
    }

    /// The usage ledger.
    #[must_use]
    pub fn usage_ledger(&self) -> PathBuf {
        self.state.join("usage.jsonl")
    }

    /// The body log, separate from the request log.
    #[must_use]
    pub fn body_log(&self) -> PathBuf {
        self.state.join("bodies.jsonl")
    }

    /// The admin socket.
    #[must_use]
    pub fn admin_socket(&self) -> PathBuf {
        self.runtime.join("admin.sock")
    }
}

/// The config file to read: `$AIRMATE_CONFIG`, else the default location.
#[must_use]
pub fn config_file(dirs: &Dirs, env: EnvFn<'_>) -> PathBuf {
    non_empty(env, "AIRMATE_CONFIG").unwrap_or_else(|| dirs.default_config_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn unix_follows_xdg_with_home_fallbacks() {
        let e = env(&[("HOME", "/home/u")]);
        let d = Dirs::resolve(Platform::Unix, &e);
        assert_eq!(d.config, PathBuf::from("/home/u/.config/airmate"));
        assert_eq!(d.state, PathBuf::from("/home/u/.local/state/airmate"));
        assert_eq!(d.runtime, PathBuf::from("/home/u/.local/state/airmate/run"));
        assert_eq!(
            d.default_config_file(),
            PathBuf::from("/home/u/.config/airmate/airmate.yaml")
        );
        let e = env(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/cfg"),
            ("XDG_STATE_HOME", "/st"),
            ("XDG_RUNTIME_DIR", "/run/user/1"),
        ]);
        let d = Dirs::resolve(Platform::Unix, &e);
        assert_eq!(
            (d.config, d.state, d.runtime),
            (
                PathBuf::from("/cfg/airmate"),
                PathBuf::from("/st/airmate"),
                PathBuf::from("/run/user/1/airmate")
            )
        );
    }

    #[test]
    fn macos_uses_application_support_unless_xdg_is_set() {
        let d = Dirs::resolve(Platform::MacOs, &env(&[("HOME", "/Users/u")]));
        assert_eq!(
            d.config,
            PathBuf::from("/Users/u/Library/Application Support/airmate")
        );
        assert_eq!(
            d.state,
            PathBuf::from("/Users/u/Library/Application Support/airmate/state")
        );
        let d = Dirs::resolve(
            Platform::MacOs,
            &env(&[("HOME", "/Users/u"), ("XDG_CONFIG_HOME", "/c")]),
        );
        assert_eq!(d.config, PathBuf::from("/c/airmate"));
    }

    #[test]
    fn windows_uses_appdata() {
        let d = Dirs::resolve(
            Platform::Windows,
            &env(&[("APPDATA", "C:/Roam"), ("LOCALAPPDATA", "C:/Local")]),
        );
        assert_eq!(d.config, PathBuf::from("C:/Roam/airmate"));
        assert_eq!(d.state, PathBuf::from("C:/Local/airmate"));
        let d = Dirs::resolve(Platform::Windows, &env(&[("USERPROFILE", "C:/Users/u")]));
        assert_eq!(
            d.config,
            PathBuf::from("C:/Users/u/AppData/Roaming/airmate")
        );
    }

    #[test]
    fn empty_variables_are_treated_as_unset_and_no_home_falls_back_to_cwd() {
        let d = Dirs::resolve(
            Platform::Unix,
            &env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "")]),
        );
        assert_eq!(d.config, PathBuf::from("/h/.config/airmate"));
        let d = Dirs::resolve(Platform::Unix, &env(&[]));
        assert_eq!(d.config, PathBuf::from("./.config/airmate"));
    }

    #[test]
    fn well_known_files_live_in_the_right_directories() {
        let d = Dirs::resolve(Platform::Unix, &env(&[("HOME", "/h")]));
        assert_eq!(
            d.token_file(),
            PathBuf::from("/h/.local/state/airmate/gateway.token")
        );
        assert_eq!(
            d.request_log(),
            PathBuf::from("/h/.local/state/airmate/requests.jsonl")
        );
        assert_eq!(
            d.usage_ledger(),
            PathBuf::from("/h/.local/state/airmate/usage.jsonl")
        );
        assert_eq!(
            d.body_log(),
            PathBuf::from("/h/.local/state/airmate/bodies.jsonl")
        );
        assert_eq!(
            d.admin_socket(),
            PathBuf::from("/h/.local/state/airmate/run/admin.sock")
        );
    }

    #[test]
    fn airmate_config_overrides_the_default_file() {
        let d = Dirs::resolve(Platform::Unix, &env(&[("HOME", "/h")]));
        assert_eq!(
            config_file(&d, &env(&[("AIRMATE_CONFIG", "/x/a.yaml")])),
            PathBuf::from("/x/a.yaml")
        );
        assert_eq!(config_file(&d, &env(&[])), d.default_config_file());
        assert_eq!(
            config_file(&d, &env(&[("AIRMATE_CONFIG", "")])),
            d.default_config_file()
        );
        let _ = Platform::current();
    }
}
