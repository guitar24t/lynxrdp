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
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                // Terminate the desktop if the session process dies unexpectedly.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
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
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
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

/// Describe a raw `wait(2)` status word in terms an administrator can act on.
///
/// The session watches the desktop with `waitpid` directly, so that `Desktop`
/// keeps ownership of its child, and what `waitpid` hands back is an encoded
/// word rather than an [`ExitStatus`]. Logging that word verbatim turned the
/// most common way there is to misconfigure this server into "status 32512" --
/// which is 127 << 8, and 127 is what a shell returns when it cannot find the
/// command it was given. A `startwm` script that does not exist is exactly what
/// a fresh installation gets wrong, so that one number is worth spelling out.
pub fn describe_wait_status(raw: i32) -> String {
    describe_exit(ExitStatus::from_raw(raw))
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

    #[test]
    fn shutdown_kills_group() {
        let tmp = tempfile::tempdir().unwrap();
        let xa = tmp.path().join("xa");
        let mut d = Desktop::spawn("sleep 30 & sleep 30", ":99", &xa, &HashMap::new()).unwrap();
        assert!(d.try_wait().unwrap().is_none());
        d.shutdown();
        assert!(d.try_wait().unwrap().is_some());
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
