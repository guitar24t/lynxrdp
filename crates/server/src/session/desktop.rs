//! Launching the desktop environment inside the session.

use std::collections::HashMap;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// A running desktop session (window manager / desktop environment).
pub struct Desktop {
    child: Child,
}

impl Desktop {
    /// Run `command` with `sh -c` on `display`, in its own process group.
    ///
    /// `extra_env` is applied on top of the current environment.
    pub fn spawn(
        command: &str,
        display: &str,
        xauth: &Path,
        extra_env: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(command)
            .env("DISPLAY", display)
            .env("XAUTHORITY", xauth)
            .env("XDG_SESSION_TYPE", "x11")
            .env("LYNXRDP_SESSION", "1")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        if let Some(home) = std::env::var_os("HOME") {
            if Path::new(&home).is_dir() {
                cmd.current_dir(home);
            }
        }
        // SAFETY: only async-signal-safe calls (setsid, prctl, fcntl) in the
        // child.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                // Terminate the desktop if the session process dies unexpectedly.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                close_inherited_fds_on_exec(None);
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("starting desktop session: {command}"))?;
        log::info!("desktop session pid {} started: {command}", child.id());
        Ok(Self { child })
    }

    /// Process id of the session leader.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Non-blocking check whether the session has ended.
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Wait for the session to end.
    pub fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait()?)
    }

    /// Terminate the whole process group, escalating to SIGKILL.
    pub fn shutdown(&mut self) {
        let pgid = self.child.id() as i32;
        // SAFETY: signalling our own child's process group.
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            // An error here is `ECHILD`: the leader was reaped by somebody
            // else, which the session's watcher used to do when its poll
            // landed first. Waiting on is then a five-second stall for a
            // status that is never coming, so it counts as gone.
            match self.child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Ok(Some(_)) | Err(_) => break,
            }
        }
        // The group rather than the leader, whether or not the leader is
        // still there: the helpers a desktop starts outlive it, and they are
        // what this call exists to end.
        // SAFETY: as above.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

impl Drop for Desktop {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            self.shutdown();
        }
    }
}

/// Mark every descriptor above stderr close-on-exec in a child between `fork`
/// and `exec`, except `keep`.
///
/// `Command` passes on whatever is not marked, and not everything a session
/// holds is: the control listener `lynxrdpd` installs as fd 3 goes through
/// `dup2`, which clears the flag, and the same is true of anything PAM or a
/// service manager opened before the session started. Every desktop process
/// inherited all of it, and a helper that outlives the desktop -- a keyring
/// daemon, a gvfsd -- then kept the session's listening socket bound after the
/// session itself was gone, which the daemon read as the session still being
/// alive and waited on.
///
/// Marked rather than closed, because the child is not alone in here: the
/// standard library's own fork-and-exec keeps a close-on-exec pipe open in the
/// child to report a failed `exec` to the parent, and closing every descriptor
/// would close that too, turning "no such program" from a spawn error into a
/// child that silently exits. Marking leaves that pipe as it was and lets the
/// `exec` do the closing. Only async-signal-safe calls, because the child may
/// have been forked from a process with other threads holding locks:
/// `close_range` where the kernel has it, otherwise `fcntl` in a loop bounded
/// by the descriptor limit.
pub(super) fn close_inherited_fds_on_exec(keep: Option<libc::c_int>) {
    match keep {
        Some(fd) if fd > 3 => {
            cloexec_fd_range(3, fd as libc::c_uint - 1);
            cloexec_fd_range(fd as libc::c_uint + 1, libc::c_uint::MAX);
        }
        Some(3) => cloexec_fd_range(4, libc::c_uint::MAX),
        _ => cloexec_fd_range(3, libc::c_uint::MAX),
    }
}

fn cloexec_fd_range(first: libc::c_uint, last: libc::c_uint) {
    if first > last {
        return;
    }
    // SAFETY: plain system calls with no memory to manage.
    unsafe {
        // Through `syscall` rather than the libc wrapper, which glibc only
        // gained in 2.34: the binary should still link on a build host older
        // than that, and it is the kernel that has to have the call anyway.
        if libc::syscall(
            libc::SYS_close_range,
            first,
            last,
            libc::CLOSE_RANGE_CLOEXEC,
        ) == 0
        {
            return;
        }
        // A kernel before 5.11 answers ENOSYS, or EINVAL for the flag. One
        // descriptor at a time, then, up to the soft limit; a limit that is
        // unlimited or absurd is capped at a count that is still a moment's
        // work, and a descriptor that is not open answers EBADF, which is
        // nothing.
        let mut limit: libc::rlimit = std::mem::zeroed();
        let ceiling = if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            limit.rlim_cur.min(1 << 16) as libc::c_uint
        } else {
            1 << 16
        };
        let mut fd = first;
        while fd < ceiling && fd <= last {
            libc::fcntl(fd as libc::c_int, libc::F_SETFD, libc::FD_CLOEXEC);
            fd += 1;
        }
    }
}

/// Describe a raw `wait(2)` status word in terms an administrator can act on.
///
/// The session watches the desktop with `waitid` directly, so that `Desktop`
/// keeps ownership of its child, and what it reports is an encoded word rather
/// than an [`ExitStatus`]. Logging that word verbatim turned the most common
/// way there is to misconfigure this server into "status 32512" -- which is
/// 127 << 8, and 127 is what a shell returns when it cannot find the command it
/// was given. A `startwm` script that does not exist is exactly what a fresh
/// installation gets wrong, so that one number is worth spelling out.
pub fn describe_wait_status(raw: i32) -> String {
    describe_exit(ExitStatus::from_raw(raw))
}

/// The `wait(2)` status word for what `waitid(2)` reported.
///
/// The session's watcher observes the desktop's exit with `waitid` and
/// `WNOWAIT`, so that observing it does not reap it: `Desktop` is the one
/// reaper, and a `try_wait` there after somebody else's `waitpid` is `ECHILD`
/// rather than a status. `waitid` reports the outcome as a code and a value,
/// and this is the word the rest of the reporting reads.
pub fn wait_status_from_siginfo(code: libc::c_int, status: libc::c_int) -> i32 {
    match code {
        libc::CLD_EXITED => (status & 0xff) << 8,
        libc::CLD_KILLED => status & 0x7f,
        libc::CLD_DUMPED => (status & 0x7f) | 0x80,
        // Stopped or continued: never asked for, so never seen.
        _ => status,
    }
}

/// Describe an [`ExitStatus`] the way [`describe_wait_status`] describes a raw
/// status word.
pub fn describe_exit(status: ExitStatus) -> String {
    if let Some(sig) = status.signal() {
        let dumped = if status.core_dumped() {
            ", core dumped"
        } else {
            ""
        };
        return format!("killed by signal {sig}{dumped}");
    }
    match status.code() {
        Some(0) => "exited normally".to_string(),
        // The shell's two "I could not run it" codes. They are the difference
        // between a wrong path and a missing execute bit, and an administrator
        // reading a log is exactly who needs to be told which one it was.
        Some(126) => "exit status 126: the desktop command was found but could not be run \
             (not executable, or its interpreter is missing)"
            .to_string(),
        Some(127) => "exit status 127: the desktop command was not found -- check that the \
             configured startwm script exists"
            .to_string(),
        Some(code) => format!("exit status {code}"),
        // Neither exited nor signalled: only a stopped child, which we never
        // ask waitpid about.
        None => format!("ended with {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_wait() {
        let tmp = tempfile::tempdir().unwrap();
        let xa = tmp.path().join("xa");
        let mut d = Desktop::spawn("exit 3", ":99", &xa, &HashMap::new()).unwrap();
        let st = d.wait().unwrap();
        assert_eq!(st.code(), Some(3));
    }

    /// Whether `pid` is gone, or a zombie nobody has collected yet -- which is
    /// dead for every purpose here, and what an orphan becomes under a PID 1
    /// that does not reap.
    fn is_dead(pid: i32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
            Err(_) => true,
        }
    }

    #[test]
    fn shutdown_kills_group() {
        let tmp = tempfile::tempdir().unwrap();
        let xa = tmp.path().join("xa");
        let pidfile = tmp.path().join("pid");
        // A background job in the leader's group, with its pid written down
        // so the test can ask after it. The leader's own exit proves nothing:
        // a signal aimed at the pid instead of the group would still end sh
        // and leave this sleep running, which is precisely what the name of
        // this test claims cannot happen.
        let cmd = format!("sleep 30 & echo $! > {}; sleep 30", pidfile.display());
        let mut d = Desktop::spawn(&cmd, ":99", &xa, &HashMap::new()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let helper: i32 = loop {
            if let Ok(pid) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = pid.trim().parse() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline, "the desktop never wrote its pid");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(!is_dead(helper));
        assert!(d.try_wait().unwrap().is_none());
        d.shutdown();
        assert!(d.try_wait().unwrap().is_some());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !is_dead(helper) {
            assert!(
                Instant::now() < deadline,
                "background job {helper} survived the group kill"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn shutdown_does_not_stall_when_the_leader_was_reaped_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let mut d =
            Desktop::spawn("sleep 30", ":99", &tmp.path().join("xa"), &HashMap::new()).unwrap();
        let pid = d.pid() as i32;
        // Somebody else collects the exit first, as the session's watcher
        // could when its poll landed before shutdown's.
        let mut status = 0;
        // SAFETY: signalling and waiting on our own child.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
        }
        let started = Instant::now();
        d.shutdown();
        // The old loop waited out its whole five-second deadline for a status
        // that had already been taken.
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn inherited_descriptors_are_closed_in_the_child() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        // A descriptor deliberately left inheritable, standing in for the
        // control listener the daemon installs with `dup2`.
        let mut fds = [0i32; 2];
        // SAFETY: fds is a valid 2-element array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let leaked = fds[1];
        let cmd = format!(
            "test -e /proc/self/fd/2 && test ! -e /proc/self/fd/{leaked} && echo closed > {out}; \
             test -e /proc/self/fd/{leaked} && echo open > {out}",
            out = out.display()
        );
        let mut d = Desktop::spawn(&cmd, ":99", &tmp.path().join("xa"), &HashMap::new()).unwrap();
        d.wait().unwrap();
        // SAFETY: closing the pipe this test created.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "closed");
    }

    #[test]
    fn wait_status_words_are_decoded() {
        // 127 << 8 is what /bin/sh reports for a startwm script that is not
        // there, and 32512 on its own told an administrator nothing at all.
        let missing = describe_wait_status(32512);
        assert!(missing.contains("127"), "{missing}");
        assert!(missing.contains("not found"), "{missing}");
        assert!(describe_wait_status(126 << 8).contains("126"));
        assert_eq!(describe_wait_status(0), "exited normally");
        assert_eq!(describe_wait_status(3 << 8), "exit status 3");
        assert_eq!(describe_wait_status(libc::SIGKILL), "killed by signal 9");
        assert!(describe_wait_status(libc::SIGSEGV | 0x80).contains("core dumped"));
    }

    #[test]
    fn siginfo_outcomes_rebuild_the_status_word() {
        assert_eq!(wait_status_from_siginfo(libc::CLD_EXITED, 127), 32512);
        assert_eq!(wait_status_from_siginfo(libc::CLD_EXITED, 0), 0);
        assert_eq!(
            describe_wait_status(wait_status_from_siginfo(libc::CLD_KILLED, libc::SIGKILL)),
            "killed by signal 9"
        );
        assert!(
            describe_wait_status(wait_status_from_siginfo(libc::CLD_DUMPED, libc::SIGSEGV))
                .contains("core dumped")
        );
    }

    #[test]
    fn a_missing_startwm_is_named_as_such() {
        // The real failure, end to end: sh cannot find the script, exits 127,
        // and the session has to say so rather than print the raw word.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("startwm.sh");
        let mut d = Desktop::spawn(
            &missing.display().to_string(),
            ":99",
            &tmp.path().join("xa"),
            &HashMap::new(),
        )
        .unwrap();
        let st = d.wait().unwrap();
        assert_eq!(st.code(), Some(127));
        assert!(describe_exit(st).contains("not found"), "{st}");
    }

    #[test]
    fn env_is_passed() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let xa = tmp.path().join("xa");
        let cmd = format!(
            "echo \"$DISPLAY $LYNXRDP_SESSION $EXTRA\" > {}",
            out.display()
        );
        let mut env = HashMap::new();
        env.insert("EXTRA".to_string(), "yes".to_string());
        let mut d = Desktop::spawn(&cmd, ":42", &xa, &env).unwrap();
        d.wait().unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), ":42 1 yes");
    }
}
