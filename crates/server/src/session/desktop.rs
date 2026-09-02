//! Launching the desktop environment inside the session.

use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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
