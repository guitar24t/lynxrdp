//! The per-session supervisor: a small root process that opens the PAM
//! session, runs `lynxrdp-session` as the user and closes the PAM session
//! when it exits. It is started by `lynxrdpd` as
//! `lynxrdpd --supervise ...` so that it is a fresh single-threaded process.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{bail, Context, Result};

use super::pam::Pam;

/// Everything the supervisor needs to know.
#[derive(Clone, Debug)]
pub struct SupervisorArgs {
    /// Target uid.
    pub uid: u32,
    /// Target primary gid.
    pub gid: u32,
    /// Login name.
    pub user: String,
    /// Home directory.
    pub home: String,
    /// Login shell.
    pub shell: String,
    /// PAM service to open a session with (`None` = skip PAM).
    pub pam_service: Option<String>,
    /// Inherited fd of the Unix listening socket for handoffs.
    pub control_fd: RawFd,
    /// Inherited fd to use as the session's stdout/stderr.
    pub log_fd: RawFd,
    /// `lynxrdp-session` executable.
    pub session_binary: PathBuf,
    /// Arguments for `lynxrdp-session` (without `--control-fd`).
    pub session_args: Vec<String>,
}

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(sig: libc::c_int) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// Build the environment for the session process.
pub fn session_env(
    args: &SupervisorArgs,
    pam_env: &[(String, String)],
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("HOME".into(), args.home.clone());
    env.insert("USER".into(), args.user.clone());
    env.insert("LOGNAME".into(), args.user.clone());
    env.insert("SHELL".into(), args.shell.clone());
    env.insert(
        "PATH".into(),
        "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin".into(),
    );
    env.insert("XDG_SESSION_TYPE".into(), "x11".into());
    env.insert("XDG_SESSION_CLASS".into(), "user".into());
    env.insert("XDG_SESSION_DESKTOP".into(), "lynxrdp".into());
    for key in [
        "LANG",
        "LANGUAGE",
        "LC_ALL",
        "LC_CTYPE",
        "LC_MESSAGES",
        "TZ",
        "RUST_LOG",
    ] {
        if let Ok(v) = std::env::var(key) {
            env.insert(key.into(), v);
        }
    }
    let runtime = format!("/run/user/{}", args.uid);
    if std::path::Path::new(&runtime).is_dir() {
        env.insert("XDG_RUNTIME_DIR".into(), runtime);
    }
    for (k, v) in pam_env {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// Run the supervisor. Returns the session's exit code.
pub fn run(args: SupervisorArgs) -> Result<i32> {
    // SAFETY: setsid has no preconditions; failure (already a leader) is fine.
    unsafe {
        libc::setsid();
    }
    let own_uid = crate::peer::own_uid();
    let need_switch = own_uid != args.uid;
    if need_switch && own_uid != 0 {
        bail!("cannot switch to uid {} without root privileges", args.uid);
    }

    // PAM session (only meaningful when we are root).
    let pam = match (&args.pam_service, own_uid) {
        (Some(service), 0) if std::path::Path::new(&format!("/etc/pam.d/{service}")).exists() => {
            match Pam::load() {
                Ok(p) => Some((p, service.clone())),
                Err(e) => {
                    log::warn!("PAM unavailable ({e}); continuing without a login session");
                    None
                }
            }
        }
        (Some(service), 0) => {
            log::warn!("/etc/pam.d/{service} not found; continuing without a PAM session");
            None
        }
        _ => None,
    };
    let mut pam_session = None;
    let mut pam_env = Vec::new();
    if let Some((pam, service)) = pam.as_ref() {
        let s = pam
            .open_session(
                service,
                &args.user,
                &[("XDG_SESSION_TYPE", "x11"), ("XDG_SESSION_CLASS", "user")],
            )
            .with_context(|| format!("opening PAM session for {}", args.user))?;
        pam_env = s.env();
        log::info!(
            "PAM session opened for {} ({} env vars)",
            args.user,
            pam_env.len()
        );
        pam_session = Some(s);
    }

    let env = session_env(&args, &pam_env);
    let mut cmd = Command::new(&args.session_binary);
    cmd.args(&args.session_args)
        .arg("--control-fd")
        .arg("3")
        .arg("--username")
        .arg(&args.user)
        .env_clear()
        .envs(&env)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if std::path::Path::new(&args.home).is_dir() {
        cmd.current_dir(&args.home);
    } else {
        cmd.current_dir("/");
    }
    let (control_fd, log_fd) = (args.control_fd, args.log_fd);
    let (uid, gid) = (args.uid, args.gid);
    let c_user = CString::new(args.user.as_str())?;
    // Captured before the fork so the child can tell whether we are still here.
    let parent_pid = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe calls in the child before exec.
    unsafe {
        cmd.pre_exec(move || {
            // Arrange fds: log -> 1,2 ; control -> 3. Move originals out of the
            // way first so the targets cannot clobber each other.
            let log_tmp = libc::fcntl(log_fd, libc::F_DUPFD, 10);
            let ctl_tmp = libc::fcntl(control_fd, libc::F_DUPFD, 10);
            if log_tmp < 0 || ctl_tmp < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(log_tmp, 1) < 0
                || libc::dup2(log_tmp, 2) < 0
                || libc::dup2(ctl_tmp, 3) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(log_tmp);
            libc::close(ctl_tmp);
            if log_fd > 3 {
                libc::close(log_fd);
            }
            if control_fd > 3 {
                libc::close(control_fd);
            }
            libc::umask(0o022);
            // Switch to the target user only when we are root and it differs
            // from us. Serving our own uid needs no change and must not try to
            // drop privileges.
            if need_switch && libc::getuid() == 0 {
                if libc::initgroups(c_user.as_ptr(), gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setgid(gid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::setuid(uid) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // When dropping to a non-root user, make sure root cannot be
                // regained.
                if uid != 0 && libc::setuid(0) == 0 {
                    return Err(std::io::Error::other("privilege drop failed"));
                }
            }
            // Die with the supervisor.
            //
            // This must come *after* the credential switch above: the kernel
            // clears the parent-death signal on any uid change (commit_creds),
            // so setting it earlier would leave it silently unset -- which is
            // precisely the state that let a SIGKILLed supervisor orphan
            // lynxrdp-session, and with it Xvfb, the desktop, and the user's
            // logind session, on an unlinked socket, indefinitely.
            //
            // Xvfb and the desktop already have this link to the session; the
            // session did not have one to the supervisor, so the chain broke at
            // exactly the point the daemon reaches for when a handoff fails.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Close the race the flag cannot: if the supervisor died between
            // the fork and the prctl above, the signal has already been sent
            // and missed, and this child would run on forever unattached.
            if libc::getppid() != parent_pid {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {}", args.session_binary.display()))?;
    CHILD_PID.store(child.id() as i32, Ordering::SeqCst);
    // SAFETY: install a signal forwarder using only async-signal-safe calls.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = forward_signal as extern "C" fn(libc::c_int) as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
    }
    // Our copies of the inherited fds are no longer needed.
    // SAFETY: closing fds we own.
    unsafe {
        libc::close(control_fd);
        libc::close(log_fd);
    }
    log::info!("session for {} running as pid {}", args.user, child.id());
    let status = child.wait().context("waiting for session")?;
    CHILD_PID.store(0, Ordering::SeqCst);
    log::info!("session for {} ended: {status}", args.user);
    if let Some(mut s) = pam_session.take() {
        s.close();
    }
    Ok(status.code().unwrap_or(128))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> SupervisorArgs {
        SupervisorArgs {
            uid: 1234,
            gid: 100,
            user: "alice".into(),
            home: "/home/alice".into(),
            shell: "/bin/bash".into(),
            pam_service: None,
            control_fd: 3,
            log_fd: 4,
            session_binary: PathBuf::from("/usr/bin/lynxrdp-session"),
            session_args: vec![],
        }
    }

    #[test]
    fn env_is_minimal_and_pam_overrides() {
        let a = args();
        let env = session_env(
            &a,
            &[
                ("XDG_RUNTIME_DIR".into(), "/run/user/1234".into()),
                ("HOME".into(), "/x".into()),
            ],
        );
        assert_eq!(env["USER"], "alice");
        assert_eq!(env["LOGNAME"], "alice");
        assert_eq!(env["HOME"], "/x");
        assert_eq!(env["XDG_RUNTIME_DIR"], "/run/user/1234");
        assert_eq!(env["XDG_SESSION_TYPE"], "x11");
        assert!(env["PATH"].contains("/usr/bin"));
        assert!(!env.contains_key("DISPLAY"));
    }
}
