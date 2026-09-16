//! The per-session supervisor: a small root process that opens the PAM
//! session, runs `lynxrdp-session` as the user and closes the PAM session
//! when it exits. It is started by `lynxrdpd` as
//! `lynxrdpd --supervise ...` so that it is a fresh single-threaded process.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::io::RawFd;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::pam::Pam;
use super::users::group_ids;

/// How long the session gets to leave after a stop signal before it is
/// killed outright.
///
/// The session's own shutdown is bounded -- five seconds for the desktop and
/// three for the X server, in `session::desktop` and `session::xserver` --
/// and this covers that with room for a loaded host. It exists because the
/// supervisor's only path to `pam_close_session` runs after the session has
/// exited, so a session that would not go was a logind session, a
/// `pam_mount` and a utmp entry that were never closed once the daemon lost
/// patience with *us*: it SIGKILLs a supervisor that has not exited within
/// its grace, and the wait for the session used to be unbounded.
///
/// `daemon::manager` sizes its own grace from this figure, which is what makes
/// the close reachable: the supervisor is always finished, session killed if
/// need be, before the daemon reaches for SIGKILL.
pub const SESSION_STOP_GRACE: Duration = Duration::from_secs(10);

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
/// Set by the handler, so the wait it interrupts knows to start the clock.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn forward_signal(sig: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
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

    // Resolve the user's groups here: in the parent, before PAM, and long
    // before the fork. `pre_exec` called `initgroups(3)`, which does this
    // lookup in the forked child, where only async-signal-safe calls are
    // allowed and NSS is emphatically not one -- and by then libpam has
    // dlopen'ed the name service stack into this process, so the very locks
    // that lookup needs may have been held by another thread at fork time.
    //
    // Doing it before `pam_open_session` as well as before the fork means a
    // failure here costs nothing: there is no session to close and nothing to
    // unwind, and the spawn simply does not happen. It must not proceed:
    // see `group_ids` for why an empty list is not an acceptable fallback.
    let groups: Vec<libc::gid_t> = if need_switch {
        group_ids(&args.user, args.gid)
            .with_context(|| format!("resolving the groups of {}", args.user))?
    } else {
        Vec::new()
    };

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
    // The change into the home directory is made in the child, after the
    // credential switch, and so with paths resolved here: between fork and
    // exec only async-signal-safe calls are allowed, and building a `CString`
    // is not one. `Command::current_dir` is not used because std applies it
    // before any `pre_exec` closure runs, which put root, not the user, into
    // the home -- and a home root cannot enter (NFS with `root_squash` and a
    // 0700 home is the ordinary case) failed the whole spawn with EACCES, for
    // a user whose SSH login worked because sshd changes directory only after
    // it has become them.
    let home_dir = CString::new(args.home.as_bytes())
        .with_context(|| format!("home directory {:?} contains a NUL", args.home))?;
    let root_dir: &'static std::ffi::CStr = c"/";
    let (control_fd, log_fd) = (args.control_fd, args.log_fd);
    let (uid, gid) = (args.uid, args.gid);
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
                // setgroups, not initgroups: the list was resolved above, in
                // the parent, and all that is left here is the bare syscall.
                if libc::setgroups(groups.len(), groups.as_ptr()) != 0 {
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
            // Into the home as the user -- see `home_dir` above. A home that
            // cannot be entered even now is not fatal: the desktop starts in
            // `/`, as a login shell would.
            if libc::chdir(home_dir.as_ptr()) != 0 && libc::chdir(root_dir.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
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
    let child = cmd
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
    let pid = child.id() as libc::pid_t;
    log::info!("session for {} running as pid {pid}", args.user);
    let status = wait_for_session(pid).context("waiting for session")?;
    CHILD_PID.store(0, Ordering::SeqCst);
    log::info!("session for {} ended: {status}", args.user);
    if let Some(mut s) = pam_session.take() {
        s.close();
    }
    Ok(status.code().unwrap_or(128))
}

/// Wait for the session to exit, and see it out if a stop signal arrives and
/// it will not go.
///
/// Not `Child::wait`: std retries that through `EINTR`, so a forwarded stop
/// signal would leave this waiting with nothing to bound the wait, which is
/// how a desktop that took its time to exit cost the user their logind
/// session. The handlers are installed without `SA_RESTART`, so a raw
/// `waitpid` returns `EINTR` when one runs, and that is the cue to start the
/// clock. A signal that lands in the few instructions between the check below
/// and the syscall is missed, and the wait is then unbounded as it always was;
/// the daemon's own SIGKILL still ends it, so that window costs nothing new.
fn wait_for_session(pid: libc::pid_t) -> std::io::Result<ExitStatus> {
    loop {
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            return reap_within(pid, SESSION_STOP_GRACE);
        }
        let mut raw = 0;
        // SAFETY: waiting on our own child; `raw` is a valid out-pointer.
        let r = unsafe { libc::waitpid(pid, &mut raw, 0) };
        if r == pid {
            return Ok(ExitStatus::from_raw(raw));
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Reap `pid`, sending SIGKILL once `grace` has passed without it exiting.
///
/// The stop signal itself has already been forwarded by the handler; this
/// only bounds how long the session gets to act on it.
fn reap_within(pid: libc::pid_t, grace: Duration) -> std::io::Result<ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        let mut raw = 0;
        // SAFETY: as in `wait_for_session`.
        let r = unsafe { libc::waitpid(pid, &mut raw, libc::WNOHANG) };
        if r == pid {
            return Ok(ExitStatus::from_raw(raw));
        }
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    log::warn!("session pid {pid} has not exited {grace:?} after a stop signal; killing it");
    // SAFETY: the pid is our own child and has not been reaped, so it cannot
    // have been reused by another process.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
    loop {
        let mut raw = 0;
        // SAFETY: as above.
        let r = unsafe { libc::waitpid(pid, &mut raw, 0) };
        if r == pid {
            return Ok(ExitStatus::from_raw(raw));
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
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
    fn the_group_list_handed_to_setgroups_is_never_empty() {
        // The child calls setgroups with exactly what this produced, so an
        // empty list would mean stripping the user of every supplementary
        // group -- video, audio, input -- rather than leaving them alone.
        let root = crate::daemon::users::user_by_uid(0).unwrap();
        let groups = group_ids(&root.name, root.gid).unwrap();
        assert!(!groups.is_empty());
        assert!(groups.contains(&root.gid), "{groups:?}");
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

    // The lint wants the `Child` waited on; the point of the helper is that
    // the code under test does that by pid instead, as `run` does.
    #[allow(clippy::zombie_processes)]
    fn sh(script: &str) -> libc::pid_t {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        // The `Child` is dropped unreaped on purpose: the code under test
        // owns the reaping, exactly as it does in `run`.
        child.id() as libc::pid_t
    }

    /// A session that does not act on its stop signal is killed once the
    /// grace has passed, so the wait ends and `pam_close_session` is reached.
    /// Before this the wait was `Child::wait`, unbounded, and the daemon's
    /// SIGKILL on the supervisor was what ended it -- with the PAM session
    /// still open.
    #[test]
    fn a_session_that_will_not_stop_is_killed_after_the_grace() {
        let pid = sh("exec sleep 30");
        let started = Instant::now();
        let status = reap_within(pid, Duration::from_millis(300)).unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// One that leaves within the grace keeps its own exit status and is not
    /// killed, and one that was never signalled at all is simply waited for.
    #[test]
    fn a_session_that_leaves_in_time_keeps_its_exit_status() {
        let status = reap_within(sh("exit 3"), Duration::from_secs(10)).unwrap();
        assert_eq!(status.code(), Some(3), "{status}");
        let status = wait_for_session(sh("exit 5")).unwrap();
        assert_eq!(status.code(), Some(5), "{status}");
    }
}
