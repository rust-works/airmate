//! `airmate service install|uninstall|start|stop|restart|status|logs`.
//!
//! The plans come from [`crate::service`]; this module carries them out, or with `--dry-run`
//! prints what they would do. Where no service manager is available a detached background process
//! stands in, with its output in the state directory.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::admin_cmd;
use super::common::{load, CliError, Global, Process, EXIT_UNHEALTHY};
use crate::config::overrides::Overrides;
use crate::service::{self, Action, FileWrite, Manager, Plan, Spec};

fn io(e: std::io::Error) -> CliError {
    CliError::runtime(format!("cannot write output: {e}"))
}

/// What to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Do {
    /// Register and start.
    Install,
    /// Stop and unregister.
    Uninstall,
    /// Start.
    Start,
    /// Stop.
    Stop,
    /// Stop, then start.
    Restart,
    /// Show whether it runs.
    Status,
    /// Show its output.
    Logs {
        /// Keep watching.
        follow: bool,
    },
}

/// Options shared by the actions.
#[derive(Debug, Clone, Copy, Default)]
pub struct Opts {
    /// Print the plan instead of carrying it out.
    pub dry_run: bool,
    /// Use this manager rather than the platform's default.
    pub manager: Option<Manager>,
}

fn spec(
    p: &Process,
    global: &Global,
) -> Result<(Spec, Manager, crate::config::paths::Dirs), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let exe = std::env::current_exe()
        .map_err(|e| CliError::runtime(format!("cannot find the airmate binary: {e}")))?;
    let mut args = vec!["serve".to_owned()];
    // The service does not inherit this shell's environment, so a config chosen by variable is
    // written into its command line.
    if let Some(c) = global
        .config
        .clone()
        .or_else(|| p.var("AIRMATE_CONFIG").map(Into::into))
    {
        let abs = std::fs::canonicalize(&c).unwrap_or(c);
        args.push("--config".to_owned());
        args.push(abs.to_string_lossy().into_owned());
    }
    if let Some(profile) = global.profile.clone().or_else(|| p.var("AIRMATE_PROFILE")) {
        args.push("--profile".to_owned());
        args.push(profile);
    }
    let cfg = &loaded.resolved.config.server;
    let home = p
        .var("HOME")
        .or_else(|| p.var("USERPROFILE"))
        .map_or_else(|| std::path::PathBuf::from("."), Into::into);
    let spec = Spec {
        exe,
        args,
        bind: cfg.bind.clone(),
        port: cfg.port,
        unix_socket: cfg.unix_socket.clone(),
        home,
        state: loaded.dirs.state.clone(),
        uid: crate::sys::uid(),
    };
    let systemd = cfg!(target_os = "linux") && Path::new("/run/systemd/system").exists();
    Ok((spec, Manager::detect(p.platform, systemd), loaded.dirs))
}

fn show_plan(plan: &Plan, out: &mut dyn Write) -> Result<(), CliError> {
    for f in &plan.files {
        writeln!(out, "would write {}:\n{}", f.path.display(), f.contents).map_err(io)?;
    }
    for s in &plan.steps {
        writeln!(out, "would run: {}", s.display()).map_err(io)?;
    }
    for r in &plan.remove {
        writeln!(out, "would remove {}", r.display()).map_err(io)?;
    }
    Ok(())
}

fn write_file(f: &FileWrite) -> Result<(), CliError> {
    if let Some(dir) = f.path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::runtime(format!("cannot create {}: {e}", dir.display())))?;
    }
    std::fs::write(&f.path, &f.contents)
        .map_err(|e| CliError::runtime(format!("cannot write {}: {e}", f.path.display())))
}

fn carry_out(plan: &Plan, out: &mut dyn Write) -> Result<(), CliError> {
    for f in &plan.files {
        write_file(f)?;
        writeln!(out, "wrote {}", f.path.display()).map_err(io)?;
    }
    for s in &plan.steps {
        let result = Command::new(&s.program)
            .args(&s.args)
            .stdin(Stdio::null())
            .output();
        match result {
            Ok(o) if o.status.success() => {}
            Ok(o) if !s.required => {
                let _ = o;
            }
            Ok(o) => {
                return Err(CliError::runtime(format!(
                    "`{}` failed ({}): {}",
                    s.display(),
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                )))
            }
            Err(e) if !s.required => {
                let _ = e;
            }
            Err(e) => {
                return Err(CliError::runtime(format!(
                    "cannot run `{}`: {e}",
                    s.display()
                )))
            }
        }
    }
    for r in &plan.remove {
        match std::fs::remove_file(r) {
            Ok(()) => writeln!(out, "removed {}", r.display()).map_err(io)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(CliError::runtime(format!(
                    "cannot remove {}: {e}",
                    r.display()
                )))
            }
        }
    }
    Ok(())
}

fn read_pid(spec: &Spec) -> Option<u32> {
    std::fs::read_to_string(spec.pid_file())
        .ok()
        .and_then(|t| service::parse_pid(&t))
}

fn running_pid(spec: &Spec) -> Option<u32> {
    read_pid(spec).filter(|p| crate::sys::process_alive(*p))
}

/// Starts a detached `airmate serve` with its output in the log file.
fn start_detached(spec: &Spec, out: &mut dyn Write) -> Result<(), CliError> {
    if let Some(pid) = running_pid(spec) {
        writeln!(out, "airmate is already running (pid {pid})").map_err(io)?;
        return Ok(());
    }
    std::fs::create_dir_all(&spec.state)
        .map_err(|e| CliError::runtime(format!("cannot create {}: {e}", spec.state.display())))?;
    let mut log = std::fs::OpenOptions::new();
    log.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log.mode(0o600);
    }
    let log = log.open(spec.log_file()).map_err(|e| {
        CliError::runtime(format!("cannot open {}: {e}", spec.log_file().display()))
    })?;
    let err_log = log
        .try_clone()
        .map_err(|e| CliError::runtime(format!("cannot open the log: {e}")))?;
    let mut cmd = Command::new(&spec.exe);
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err_log);
    // Its own process group, so Ctrl-C in this terminal does not reach it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd
        .spawn()
        .map_err(|e| CliError::runtime(format!("cannot start {}: {e}", spec.exe.display())))?;
    std::fs::write(spec.pid_file(), child.id().to_string())
        .map_err(|e| CliError::runtime(format!("cannot write the pid file: {e}")))?;
    writeln!(
        out,
        "started airmate (pid {}), logging to {}",
        child.id(),
        spec.log_file().display()
    )
    .map_err(io)
}

fn stop_detached(spec: &Spec, out: &mut dyn Write) -> Result<(), CliError> {
    let Some(pid) = running_pid(spec) else {
        let _ = std::fs::remove_file(spec.pid_file());
        writeln!(out, "airmate is not running").map_err(io)?;
        return Ok(());
    };
    if !crate::sys::terminate(pid) {
        return Err(CliError::runtime(format!("cannot signal pid {pid}")));
    }
    let deadline = Instant::now() + Duration::from_secs(40);
    while crate::sys::process_alive(pid) {
        if Instant::now() > deadline {
            return Err(CliError::runtime(format!(
                "airmate (pid {pid}) is still draining requests; it will exit when they finish"
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(spec.pid_file());
    writeln!(out, "stopped airmate (pid {pid})").map_err(io)
}

fn tail_file(path: &Path, follow: bool, out: &mut dyn Write) -> Result<(), CliError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(CliError::runtime(format!(
                "no service log at {} yet",
                path.display()
            )))
        }
        Err(e) => {
            return Err(CliError::runtime(format!(
                "cannot read {}: {e}",
                path.display()
            )))
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    for l in &lines[lines.len().saturating_sub(200)..] {
        writeln!(out, "{l}").map_err(io)?;
    }
    if follow {
        let mut pos = text.len() as u64;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let Ok(meta) = std::fs::metadata(path) else {
                continue;
            };
            if meta.len() < pos {
                pos = 0;
            }
            if meta.len() > pos {
                let Ok(bytes) = std::fs::read(path) else {
                    continue;
                };
                let new = &bytes[usize::try_from(pos).unwrap_or(0).min(bytes.len())..];
                if out.write_all(new).is_err() || out.flush().is_err() {
                    return Ok(());
                }
                pos = bytes.len() as u64;
            }
        }
    }
    Ok(())
}

/// Runs one service action.
///
/// # Errors
///
/// A configuration problem, a failed service-manager command, or (for `status`) exit 3 when the
/// gateway is not running.
pub fn run(
    p: &Process,
    global: &Global,
    what: Do,
    opts: Opts,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let (spec, detected, dirs) = spec(p, global)?;
    let manager = opts.manager.unwrap_or(detected);
    let action = match what {
        Do::Install => Some(Action::Install),
        Do::Uninstall => Some(Action::Uninstall),
        Do::Start => Some(Action::Start),
        Do::Stop => Some(Action::Stop),
        Do::Restart => Some(Action::Restart),
        Do::Status | Do::Logs { .. } => None,
    };
    if let Some(action) = action {
        if manager == Manager::Detached {
            if opts.dry_run {
                return writeln!(
                    out,
                    "would {} a detached process: {} {}",
                    match action {
                        Action::Install | Action::Start => "start",
                        Action::Uninstall | Action::Stop => "stop",
                        Action::Restart => "restart",
                    },
                    spec.exe.display(),
                    spec.args.join(" ")
                )
                .map_err(io);
            }
            return match action {
                Action::Install | Action::Start => start_detached(&spec, out),
                Action::Uninstall | Action::Stop => stop_detached(&spec, out),
                Action::Restart => {
                    stop_detached(&spec, out)?;
                    start_detached(&spec, out)
                }
            };
        }
        let plan = service::plan(manager, action, &spec);
        if opts.dry_run {
            return show_plan(&plan, out);
        }
        carry_out(&plan, out)?;
        let verb = match action {
            Action::Install => "installed and started",
            Action::Uninstall => "uninstalled",
            Action::Start => "started",
            Action::Stop => "stopped",
            Action::Restart => "restarted",
        };
        return writeln!(out, "airmate service {verb}").map_err(io);
    }
    match what {
        Do::Logs { follow } => {
            if let Some(step) = service::logs_command(manager, follow) {
                let status = Command::new(&step.program)
                    .args(&step.args)
                    .status()
                    .map_err(|e| {
                        CliError::runtime(format!("cannot run `{}`: {e}", step.display()))
                    })?;
                return if status.success() {
                    Ok(())
                } else {
                    Err(CliError::runtime(format!(
                        "`{}` failed ({status})",
                        step.display()
                    )))
                };
            }
            tail_file(&spec.log_file(), follow, out)
        }
        _ => {
            let installed = match manager {
                Manager::Launchd => spec
                    .plist_path()
                    .exists()
                    .then(|| spec.plist_path().display().to_string()),
                Manager::Systemd => {
                    spec.systemd_dir()
                        .join("airmate.service")
                        .exists()
                        .then(|| {
                            spec.systemd_dir()
                                .join("airmate.service")
                                .display()
                                .to_string()
                        })
                }
                Manager::Detached => {
                    running_pid(&spec).map(|pid| format!("detached process, pid {pid}"))
                }
                Manager::Task => None,
            };
            writeln!(
                out,
                "manager: {}\ninstalled: {}",
                match manager {
                    Manager::Launchd => "launchd",
                    Manager::Systemd => "systemd",
                    Manager::Detached => "detached process",
                    Manager::Task => "scheduled task",
                },
                installed.as_deref().unwrap_or("no")
            )
            .map_err(io)?;
            match admin_cmd::call(&dirs.admin_socket(), "GET", "/status") {
                Ok((200, _)) => writeln!(out, "running: yes (see `airmate status`)").map_err(io),
                _ => {
                    writeln!(out, "running: no").map_err(io)?;
                    let mut e = CliError::runtime("airmate is not running");
                    e.code = EXIT_UNHEALTHY;
                    Err(e)
                }
            }
        }
    }
}
