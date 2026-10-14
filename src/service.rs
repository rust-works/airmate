//! Running airmate as a per-user service.
//!
//! This module plans: given where the binary and its files live, it says which files to write and
//! which commands to run for each service manager. Nothing here touches the machine, so every plan
//! can be checked in a test and shown with `--dry-run`; `cli::service_cmd` carries a plan out.
//!
//! - macOS: a launchd agent that owns the listening socket, so the gateway starts on first
//!   connect and at login.
//! - Linux with systemd: a user socket unit and service unit, the same arrangement.
//! - Anywhere else: a detached background process with its output in the state directory.
//! - Windows: a user-level scheduled task that starts the gateway at logon.

use std::path::PathBuf;

use crate::config::paths::Platform;

/// The launchd label and the base of the Windows task name.
pub const LABEL: &str = "dev.airmate.gateway";
/// The systemd unit name, without the suffix.
pub const UNIT: &str = "airmate";

/// How the gateway is kept running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    /// A launchd user agent.
    Launchd,
    /// systemd user units.
    Systemd,
    /// A detached process, no service manager.
    Detached,
    /// A Windows scheduled task.
    Task,
}

impl Manager {
    /// What a platform uses by default. `systemd_present` is whether a systemd user session
    /// exists on this machine.
    #[must_use]
    pub fn detect(platform: Platform, systemd_present: bool) -> Self {
        match platform {
            Platform::MacOs => Self::Launchd,
            Platform::Windows => Self::Task,
            Platform::Unix if systemd_present => Self::Systemd,
            Platform::Unix => Self::Detached,
        }
    }
}

/// What the service needs to know about the installation.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The airmate binary.
    pub exe: PathBuf,
    /// Arguments after the binary: `serve`, plus `--config` and `--profile` when given.
    pub args: Vec<String>,
    /// The bind address from the configuration.
    pub bind: String,
    /// The port from the configuration.
    pub port: u16,
    /// A Unix socket from the configuration; such a service owns its own socket.
    pub unix_socket: Option<String>,
    /// The user's home directory.
    pub home: PathBuf,
    /// The state directory, where the log and pid file live.
    pub state: PathBuf,
    /// The user id, for launchd's `gui/<uid>` domain.
    pub uid: u32,
}

impl Spec {
    /// The log file the detached and launchd forms write to.
    #[must_use]
    pub fn log_file(&self) -> PathBuf {
        self.state.join("service.log")
    }

    /// The pid file of a detached process.
    #[must_use]
    pub fn pid_file(&self) -> PathBuf {
        self.state.join("airmate.pid")
    }

    /// The launchd agent file.
    #[must_use]
    pub fn plist_path(&self) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    /// The directory systemd user units live in.
    #[must_use]
    pub fn systemd_dir(&self) -> PathBuf {
        self.home.join(".config/systemd/user")
    }

    fn listen_address(&self) -> String {
        if self.bind.contains(':') {
            format!("[{}]:{}", self.bind, self.port)
        } else {
            format!("{}:{}", self.bind, self.port)
        }
    }
}

/// A file to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileWrite {
    /// Where.
    pub path: PathBuf,
    /// What.
    pub contents: String,
}

/// A command to run. A failure of `required: false` steps is reported but does not stop the plan:
/// stopping something that is not running, say, is not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The program.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Whether the plan stops when this step fails.
    pub required: bool,
}

impl Step {
    fn new(program: &str, args: &[&str], required: bool) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            required,
        }
    }

    /// The command as one line, for `--dry-run`.
    #[must_use]
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(|p| {
                if p.contains(' ') {
                    format!("{p:?}")
                } else {
                    p.to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// What a service action does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Files to write first.
    pub files: Vec<FileWrite>,
    /// Then commands to run, in order.
    pub steps: Vec<Step>,
    /// Files to remove after the commands.
    pub remove: Vec<PathBuf>,
}

/// An action on the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Register the service and start it.
    Install,
    /// Stop it and remove the registration.
    Uninstall,
    /// Start it.
    Start,
    /// Stop it.
    Stop,
    /// Stop and start it.
    Restart,
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The launchd agent. The agent declares the listening socket itself, so launchd holds the port
/// while the gateway is down and starts it when something connects.
#[must_use]
pub fn launchd_plist(spec: &Spec) -> String {
    let args: String = std::iter::once(spec.exe.to_string_lossy().into_owned())
        .chain(spec.args.iter().cloned())
        .map(|a| format!("    <string>{}</string>\n", xml(&a)))
        .collect();
    let socket = match &spec.unix_socket {
        Some(_) => String::new(),
        None => format!(
            "  <key>Sockets</key>\n  <dict>\n    <key>Listeners</key>\n    <dict>\n      <key>SockNodeName</key>\n      <string>{}</string>\n      <key>SockServiceName</key>\n      <string>{}</string>\n    </dict>\n  </dict>\n",
            xml(&spec.bind),
            spec.port
        ),
    };
    let log = xml(&spec.log_file().to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{LABEL}</string>\n  <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n{socket}  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n  <key>ProcessType</key>\n  <string>Background</string>\n  <key>StandardOutPath</key>\n  <string>{log}</string>\n  <key>StandardErrorPath</key>\n  <string>{log}</string>\n</dict>\n</plist>\n"
    )
}

/// systemd quoting for one `ExecStart` word.
fn systemd_word(w: &str) -> String {
    if w.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-=:@".contains(c))
    {
        w.to_owned()
    } else {
        format!("\"{}\"", w.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// The systemd socket unit.
#[must_use]
pub fn systemd_socket(spec: &Spec) -> String {
    format!(
        "[Unit]\nDescription=airmate gateway socket\n\n[Socket]\nListenStream={}\nNoDelay=true\n\n[Install]\nWantedBy=sockets.target\n",
        spec.listen_address()
    )
}

/// The systemd service unit.
#[must_use]
pub fn systemd_service(spec: &Spec) -> String {
    let exec: Vec<String> = std::iter::once(spec.exe.to_string_lossy().into_owned())
        .chain(spec.args.iter().cloned())
        .map(|w| systemd_word(&w))
        .collect();
    // With a socket unit, enabling the socket is what starts the service; without one the service
    // is enabled directly.
    let (unit_extra, install) = if spec.unix_socket.is_some() {
        (
            String::new(),
            "\n[Install]\nWantedBy=default.target\n".to_owned(),
        )
    } else {
        (
            format!("Requires={UNIT}.socket\nAfter={UNIT}.socket\n"),
            String::new(),
        )
    };
    format!(
        "[Unit]\nDescription=airmate gateway\n{unit_extra}\n[Service]\nExecStart={}\nRestart=on-failure\nRestartSec=2\nNoNewPrivileges=true\nPrivateTmp=true\nProtectSystem=strict\nReadWritePaths={}\n{install}",
        exec.join(" "),
        systemd_word(&spec.state.to_string_lossy()),
    )
}

fn windows_task_command(spec: &Spec) -> String {
    let mut parts = vec![format!("\"{}\"", spec.exe.display())];
    parts.extend(spec.args.iter().map(|a| {
        if a.contains(' ') {
            format!("\"{a}\"")
        } else {
            a.clone()
        }
    }));
    parts.join(" ")
}

/// The plan for `action` under `manager`.
#[must_use]
pub fn plan(manager: Manager, action: Action, spec: &Spec) -> Plan {
    let mut p = Plan::default();
    match manager {
        Manager::Launchd => {
            let domain = format!("gui/{}", spec.uid);
            let target = format!("{domain}/{LABEL}");
            let plist = spec.plist_path();
            let plist_str = plist.to_string_lossy().into_owned();
            match action {
                Action::Install => {
                    p.files.push(FileWrite {
                        path: plist,
                        contents: launchd_plist(spec),
                    });
                    // A previous registration is removed first so a changed plist takes effect.
                    p.steps
                        .push(Step::new("launchctl", &["bootout", &target], false));
                    p.steps.push(Step::new(
                        "launchctl",
                        &["bootstrap", &domain, &plist_str],
                        true,
                    ));
                }
                Action::Uninstall => {
                    p.steps
                        .push(Step::new("launchctl", &["bootout", &target], false));
                    p.remove.push(plist);
                }
                Action::Start => {
                    p.steps
                        .push(Step::new("launchctl", &["kickstart", &target], true))
                }
                Action::Stop => {
                    p.steps
                        .push(Step::new("launchctl", &["kill", "SIGTERM", &target], true))
                }
                Action::Restart => {
                    p.steps
                        .push(Step::new("launchctl", &["kickstart", "-k", &target], true))
                }
            }
        }
        Manager::Systemd => {
            let socket = format!("{UNIT}.socket");
            let service = format!("{UNIT}.service");
            let dir = spec.systemd_dir();
            let has_socket = spec.unix_socket.is_none();
            let primary = if has_socket {
                socket.clone()
            } else {
                service.clone()
            };
            match action {
                Action::Install => {
                    if has_socket {
                        p.files.push(FileWrite {
                            path: dir.join(&socket),
                            contents: systemd_socket(spec),
                        });
                    }
                    p.files.push(FileWrite {
                        path: dir.join(&service),
                        contents: systemd_service(spec),
                    });
                    p.steps
                        .push(Step::new("systemctl", &["--user", "daemon-reload"], true));
                    p.steps.push(Step::new(
                        "systemctl",
                        &["--user", "enable", "--now", &primary],
                        true,
                    ));
                }
                Action::Uninstall => {
                    p.steps.push(Step::new(
                        "systemctl",
                        &["--user", "disable", "--now", &primary],
                        false,
                    ));
                    p.steps
                        .push(Step::new("systemctl", &["--user", "stop", &service], false));
                    p.remove.push(dir.join(&service));
                    if has_socket {
                        p.remove.push(dir.join(&socket));
                    }
                    p.steps
                        .push(Step::new("systemctl", &["--user", "daemon-reload"], false));
                }
                Action::Start => {
                    p.steps
                        .push(Step::new("systemctl", &["--user", "start", &primary], true))
                }
                Action::Stop => {
                    p.steps
                        .push(Step::new("systemctl", &["--user", "stop", &service], true));
                    if has_socket {
                        p.steps
                            .push(Step::new("systemctl", &["--user", "stop", &socket], false));
                    }
                }
                Action::Restart => p.steps.push(Step::new(
                    "systemctl",
                    &["--user", "restart", &service],
                    true,
                )),
            }
        }
        Manager::Task => {
            let tr = windows_task_command(spec);
            match action {
                Action::Install => {
                    p.steps.push(Step::new(
                        "schtasks",
                        &["/Create", "/F", "/SC", "ONLOGON", "/TN", LABEL, "/TR", &tr],
                        true,
                    ));
                    p.steps
                        .push(Step::new("schtasks", &["/Run", "/TN", LABEL], true));
                }
                Action::Uninstall => {
                    p.steps
                        .push(Step::new("schtasks", &["/End", "/TN", LABEL], false));
                    p.steps.push(Step::new(
                        "schtasks",
                        &["/Delete", "/F", "/TN", LABEL],
                        true,
                    ));
                }
                Action::Start => p
                    .steps
                    .push(Step::new("schtasks", &["/Run", "/TN", LABEL], true)),
                Action::Stop => p
                    .steps
                    .push(Step::new("schtasks", &["/End", "/TN", LABEL], true)),
                Action::Restart => {
                    p.steps
                        .push(Step::new("schtasks", &["/End", "/TN", LABEL], false));
                    p.steps
                        .push(Step::new("schtasks", &["/Run", "/TN", LABEL], true));
                }
            }
        }
        // A detached process is not a command plan; `service_cmd` spawns and signals it directly.
        Manager::Detached => {}
    }
    p
}

/// The program and arguments that print the service's recent output, if the manager keeps it
/// somewhere other than the log file.
#[must_use]
pub fn logs_command(manager: Manager, follow: bool) -> Option<Step> {
    match manager {
        Manager::Systemd => {
            let mut args = vec!["--user", "-u", "airmate.service", "--no-pager"];
            if follow {
                args.push("-f");
            }
            Some(Step::new("journalctl", &args, true))
        }
        _ => None,
    }
}

/// A detached process's pid, read back from its pid file.
#[must_use]
pub fn parse_pid(text: &str) -> Option<u32> {
    text.trim().parse().ok().filter(|p| *p > 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            exe: PathBuf::from("/opt/airmate/bin/airmate"),
            args: vec!["serve".into()],
            bind: "127.0.0.1".into(),
            port: 9997,
            unix_socket: None,
            home: PathBuf::from("/home/u"),
            state: PathBuf::from("/home/u/.local/state/airmate"),
            uid: 501,
        }
    }

    #[test]
    fn the_manager_follows_the_platform() {
        assert_eq!(Manager::detect(Platform::MacOs, false), Manager::Launchd);
        assert_eq!(Manager::detect(Platform::Windows, false), Manager::Task);
        assert_eq!(Manager::detect(Platform::Unix, true), Manager::Systemd);
        assert_eq!(Manager::detect(Platform::Unix, false), Manager::Detached);
    }

    #[cfg(unix)]
    #[test]
    fn the_launchd_agent_owns_the_socket_and_runs_at_load() {
        let p = launchd_plist(&spec());
        for want in [
            "<string>dev.airmate.gateway</string>",
            "<string>/opt/airmate/bin/airmate</string>",
            "<string>serve</string>",
            "<key>Listeners</key>",
            "<key>SockNodeName</key>\n      <string>127.0.0.1</string>",
            "<key>SockServiceName</key>\n      <string>9997</string>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<key>SuccessfulExit</key>\n    <false/>",
            "/home/u/.local/state/airmate/service.log",
        ] {
            assert!(p.contains(want), "missing {want:?}\n{p}");
        }
        assert!(p.starts_with("<?xml") && p.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn special_characters_in_paths_are_escaped_in_the_plist() {
        let mut s = spec();
        s.exe = PathBuf::from("/Apps/R&D <test>/airmate");
        let p = launchd_plist(&s);
        assert!(p.contains("/Apps/R&amp;D &lt;test&gt;/airmate"), "{p}");
        assert!(!p.contains("R&D"));
    }

    #[test]
    fn a_unix_socket_config_gets_no_launchd_socket_entry() {
        let mut s = spec();
        s.unix_socket = Some("/run/user/1000/airmate.sock".into());
        assert!(!launchd_plist(&s).contains("Sockets"));
    }

    #[test]
    fn the_systemd_units_are_socket_activated() {
        let s = spec();
        assert_eq!(
            systemd_socket(&s),
            "[Unit]\nDescription=airmate gateway socket\n\n[Socket]\nListenStream=127.0.0.1:9997\nNoDelay=true\n\n[Install]\nWantedBy=sockets.target\n"
        );
        let svc = systemd_service(&s);
        for want in [
            "ExecStart=/opt/airmate/bin/airmate serve",
            "Requires=airmate.socket",
            "After=airmate.socket",
            "Restart=on-failure",
            "NoNewPrivileges=true",
            "ReadWritePaths=/home/u/.local/state/airmate",
        ] {
            assert!(svc.contains(want), "missing {want:?}\n{svc}");
        }
        assert!(
            !svc.contains("[Install]"),
            "the socket is what gets enabled:\n{svc}"
        );
    }

    #[test]
    fn an_ipv6_bind_is_bracketed_and_awkward_paths_are_quoted() {
        let mut s = spec();
        s.bind = "::1".into();
        assert!(systemd_socket(&s).contains("ListenStream=[::1]:9997"));
        s.exe = PathBuf::from("/opt/my airmate/airmate");
        s.args = vec!["serve".into(), "--config".into(), "/c/my conf.yaml".into()];
        assert!(systemd_service(&s)
            .contains("ExecStart=\"/opt/my airmate/airmate\" serve --config \"/c/my conf.yaml\""));
    }

    #[test]
    fn a_unix_socket_systemd_service_runs_on_its_own() {
        let mut s = spec();
        s.unix_socket = Some("/run/airmate.sock".into());
        let svc = systemd_service(&s);
        assert!(
            !svc.contains("airmate.socket") && svc.contains("[Install]\nWantedBy=default.target"),
            "{svc}"
        );
        let p = plan(Manager::Systemd, Action::Install, &s);
        assert_eq!(p.files.len(), 1);
        assert_eq!(
            p.steps.last().unwrap().display(),
            "systemctl --user enable --now airmate.service"
        );
    }

    #[cfg(unix)]
    #[test]
    fn launchd_install_replaces_then_bootstraps_and_uninstall_removes_the_agent() {
        let s = spec();
        let i = plan(Manager::Launchd, Action::Install, &s);
        assert_eq!(
            i.files[0].path,
            PathBuf::from("/home/u/Library/LaunchAgents/dev.airmate.gateway.plist")
        );
        assert_eq!(
            i.steps.iter().map(|s| (s.display(), s.required)).collect::<Vec<_>>(),
            [
                ("launchctl bootout gui/501/dev.airmate.gateway".to_owned(), false),
                ("launchctl bootstrap gui/501 /home/u/Library/LaunchAgents/dev.airmate.gateway.plist".to_owned(), true),
            ]
        );
        let u = plan(Manager::Launchd, Action::Uninstall, &s);
        assert_eq!(u.remove, [s.plist_path()]);
        assert!(
            !u.steps[0].required,
            "removing what is not loaded is not a failure"
        );
        assert_eq!(
            plan(Manager::Launchd, Action::Restart, &s).steps[0].display(),
            "launchctl kickstart -k gui/501/dev.airmate.gateway"
        );
        assert_eq!(
            plan(Manager::Launchd, Action::Stop, &s).steps[0].display(),
            "launchctl kill SIGTERM gui/501/dev.airmate.gateway"
        );
        assert_eq!(
            plan(Manager::Launchd, Action::Start, &s).steps[0].display(),
            "launchctl kickstart gui/501/dev.airmate.gateway"
        );
    }

    #[test]
    fn systemd_install_writes_both_units_and_enables_the_socket() {
        let s = spec();
        let i = plan(Manager::Systemd, Action::Install, &s);
        assert_eq!(
            i.files
                .iter()
                .map(|f| f.path.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["airmate.socket", "airmate.service"]
        );
        assert_eq!(
            i.steps.iter().map(Step::display).collect::<Vec<_>>(),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now airmate.socket"
            ]
        );
        let u = plan(Manager::Systemd, Action::Uninstall, &s);
        assert_eq!(u.remove.len(), 2);
        assert!(u.steps.iter().take(2).all(|s| !s.required));
        let stop = plan(Manager::Systemd, Action::Stop, &s);
        assert_eq!(
            stop.steps.iter().map(Step::display).collect::<Vec<_>>(),
            [
                "systemctl --user stop airmate.service",
                "systemctl --user stop airmate.socket"
            ]
        );
    }

    #[test]
    fn the_windows_task_runs_at_logon() {
        let mut s = spec();
        s.exe = PathBuf::from(r"C:\Program Files\airmate\airmate.exe");
        s.args = vec![
            "serve".into(),
            "--config".into(),
            r"C:\Users\u\my conf.yaml".into(),
        ];
        let i = plan(Manager::Task, Action::Install, &s);
        let create = &i.steps[0];
        assert_eq!(create.program, "schtasks");
        assert_eq!(
            &create.args[..6],
            ["/Create", "/F", "/SC", "ONLOGON", "/TN", LABEL]
        );
        assert_eq!(
            create.args[7],
            r#""C:\Program Files\airmate\airmate.exe" serve --config "C:\Users\u\my conf.yaml""#
        );
        assert_eq!(
            i.steps[1].display(),
            "schtasks /Run /TN dev.airmate.gateway"
        );
        assert!(i.files.is_empty());
    }

    #[test]
    fn a_detached_process_has_no_command_plan() {
        for a in [
            Action::Install,
            Action::Uninstall,
            Action::Start,
            Action::Stop,
            Action::Restart,
        ] {
            assert_eq!(plan(Manager::Detached, a, &spec()), Plan::default());
        }
    }

    #[test]
    fn logs_come_from_the_journal_only_under_systemd() {
        assert_eq!(
            logs_command(Manager::Systemd, true).unwrap().display(),
            "journalctl --user -u airmate.service --no-pager -f"
        );
        assert_eq!(
            logs_command(Manager::Systemd, false).unwrap().display(),
            "journalctl --user -u airmate.service --no-pager"
        );
        assert!(logs_command(Manager::Launchd, false).is_none());
    }

    #[test]
    fn pids_parse_and_nonsense_does_not() {
        assert_eq!(parse_pid("1234\n"), Some(1234));
        for bad in ["", "abc", "0", "1", "-5"] {
            assert_eq!(parse_pid(bad), None, "{bad:?}");
        }
    }
}
