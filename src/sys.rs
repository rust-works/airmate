//! The few places where airmate talks to the operating system below what std and tokio offer:
//! the file-descriptor limit, private Unix-domain sockets, and sockets handed over by a service
//! manager. All the `unsafe` in the crate is here, each use with the reason it is sound.

use std::path::Path;

/// Raises the soft `RLIMIT_NOFILE` toward `target`, never past the hard limit. Returns the
/// `(old, new)` soft limits when it changed them.
#[cfg(unix)]
#[must_use]
// `rlim_t` is `u64` on the platforms CI covers but narrower elsewhere, so the conversions stay.
#[allow(clippy::useless_conversion)]
pub fn raise_fd_limit(target: u64) -> Option<(u64, u64)> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid, writable `rlimit` for the duration of the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut lim) } != 0 {
        return None;
    }
    let old = u64::from(lim.rlim_cur);
    let hard = u64::from(lim.rlim_max);
    let want = target.min(hard);
    if old >= want {
        return None;
    }
    // macOS refuses a soft limit above OPEN_MAX even when the hard limit is "infinite", so try
    // the wanted value first and settle for what the kernel allows.
    for candidate in [want, want.min(10_240)] {
        let new = libc::rlimit {
            rlim_cur: candidate as libc::rlim_t,
            rlim_max: lim.rlim_max,
        };
        // SAFETY: `new` is a valid `rlimit`; setrlimit only reads it.
        if candidate > old && unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raw const new) } == 0 {
            return Some((old, candidate));
        }
    }
    None
}

/// No-op where there is no `RLIMIT_NOFILE`.
#[cfg(not(unix))]
#[must_use]
pub fn raise_fd_limit(_: u64) -> Option<(u64, u64)> {
    None
}

/// Binds a Unix-domain socket that only the owner can use, in a directory only the owner can
/// enter. A socket file left by a dead process is replaced; one that still answers belongs to a
/// running process and is refused.
///
/// # Errors
///
/// The directory or socket cannot be created, or the path is in use.
#[cfg(unix)]
pub fn bind_private_socket(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("another airmate is already serving {}", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// The listening TCP socket a service manager passed in, if one did: systemd's `LISTEN_FDS`
/// protocol (the first socket is file descriptor 3, and `LISTEN_PID` must name this process) or
/// launchd's `Listeners` socket entry. `None` when started any other way.
#[cfg(unix)]
#[must_use]
pub fn inherited_listener() -> Option<std::net::TcpListener> {
    let fd = systemd_fd(
        std::env::var("LISTEN_PID").ok().as_deref(),
        std::env::var("LISTEN_FDS").ok().as_deref(),
        std::process::id(),
    )
    .or_else(launchd_fd)?;
    use std::os::fd::FromRawFd;
    // SAFETY: the service manager handed this descriptor to us to own, and nothing else in the
    // process has taken it; wrapping it once makes this the only owner.
    let listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    listener.set_nonblocking(true).ok()?;
    Some(listener)
}

/// No sockets are inherited where there is no service manager protocol for it.
#[cfg(not(unix))]
#[must_use]
pub fn inherited_listener() -> Option<std::net::TcpListener> {
    None
}

/// Reads systemd's socket-activation variables. `pid` and `fds` are `LISTEN_PID` and
/// `LISTEN_FDS`; the descriptor is only ours when `pid` is this process.
#[must_use]
pub fn systemd_fd(pid: Option<&str>, fds: Option<&str>, me: u32) -> Option<i32> {
    let pid: u32 = pid?.trim().parse().ok()?;
    let fds: i32 = fds?.trim().parse().ok()?;
    (pid == me && fds >= 1).then_some(3)
}

#[cfg(target_os = "macos")]
fn launchd_fd() -> Option<i32> {
    extern "C" {
        fn launch_activate_socket(
            name: *const std::ffi::c_char,
            fds: *mut *mut libc::c_int,
            cnt: *mut libc::size_t,
        ) -> libc::c_int;
    }
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut cnt: libc::size_t = 0;
    // SAFETY: the name is a NUL-terminated literal and the out-parameters are valid pointers. On
    // success launchd allocates `fds` with malloc, which this code frees once the first descriptor
    // has been copied out.
    unsafe {
        if launch_activate_socket(c"Listeners".as_ptr(), &raw mut fds, &raw mut cnt) != 0
            || cnt == 0
            || fds.is_null()
        {
            return None;
        }
        let fd = *fds;
        libc::free(fds.cast());
        Some(fd)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn launchd_fd() -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_hands_over_descriptor_three_only_to_the_process_it_names() {
        assert_eq!(systemd_fd(Some("42"), Some("1"), 42), Some(3));
        assert_eq!(
            systemd_fd(Some("42"), Some("2"), 42),
            Some(3),
            "the first of several"
        );
        assert_eq!(
            systemd_fd(Some("41"), Some("1"), 42),
            None,
            "someone else's sockets"
        );
        assert_eq!(systemd_fd(Some("42"), Some("0"), 42), None);
        assert_eq!(systemd_fd(None, Some("1"), 42), None);
        assert_eq!(systemd_fd(Some("42"), None, 42), None);
        assert_eq!(systemd_fd(Some("x"), Some("1"), 42), None);
    }

    #[cfg(unix)]
    #[test]
    #[allow(clippy::useless_conversion)]
    fn the_fd_limit_only_ever_goes_up() {
        let before = unsafe {
            let mut l = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut l);
            u64::from(l.rlim_cur)
        };
        if let Some((old, new)) = raise_fd_limit(before + 16) {
            assert_eq!(old, before);
            assert!(new > old);
        }
        assert_eq!(raise_fd_limit(1), None, "a lower target changes nothing");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn private_sockets_are_owner_only_and_stale_ones_are_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::path::PathBuf::from(format!("/tmp/am-sys-{}-{nanos}", std::process::id()));
        let path = dir.join("run/s.sock");
        let live = bind_private_socket(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let err = bind_private_socket(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
        drop(live);
        assert!(
            path.exists(),
            "dropping the listener leaves the file, as a crash would"
        );
        // The kernel finishes closing a listener a moment after the drop; a dead process's socket
        // is likewise refused only once that has happened.
        for _ in 0..200 {
            if std::os::unix::net::UnixStream::connect(&path).is_err() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let again = bind_private_socket(&path).expect("a stale socket is replaced");
        drop(again);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
