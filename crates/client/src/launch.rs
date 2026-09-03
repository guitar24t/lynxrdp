//! Starting a session from the launcher.
//!
//! The launcher runs its own winit event loop through eframe, and a session
//! window needs one too. A process may only have one, so a session runs as a
//! child process: this binary, invoked again with the profile's arguments.
//!
//! That is not a workaround so much as the right shape. Several sessions can
//! be open at once, a session that crashes cannot take the launcher with it,
//! and the arguments a profile produces are exactly the ones a user could
//! have typed, so the two entry points cannot drift apart.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result};

use crate::profiles::Profile;

/// Sessions started by this launcher, kept so they can be reaped.
///
/// A child that is never waited for stays a zombie on Unix. Nothing here
/// needs the exit status, but something has to collect it.
#[derive(Debug, Default)]
pub struct Sessions {
    running: Vec<Child>,
}

impl Sessions {
    /// Start `profile` by re-invoking this executable.
    pub fn start(&mut self, profile: &Profile) -> Result<()> {
        let program = std::env::current_exe().context("finding this executable")?;
        self.start_with(&program, profile)
    }

    /// Start using an explicit program, which is what the tests use.
    pub fn start_with(&mut self, program: &PathBuf, profile: &Profile) -> Result<()> {
        let mut command = Command::new(program);
        command
            .args(profile.args())
            // The session draws its own window; it has no use for our
            // streams, and inheriting them would keep a terminal busy.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_console_window(&mut command);
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        self.running.push(child);
        Ok(())
    }

    /// Collect any sessions that have exited.
    ///
    /// Called from the launcher's repaint, which is frequent and must not
    /// block, so this only ever polls.
    pub fn reap(&mut self) {
        self.running
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
    }

    /// How many sessions this launcher started are still running.
    pub fn count(&mut self) -> usize {
        self.reap();
        self.running.len()
    }
}

/// Keep Windows from opening a console window for the child.
///
/// The launcher is a windowed application; a console flashing up behind each
/// session would look broken. Nothing to do on other platforms.
#[cfg(windows)]
fn no_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    /// CREATE_NO_WINDOW
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn no_console_window(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        let mut p = Profile::new("test");
        p.host = "example.org".into();
        p
    }

    #[cfg(unix)]
    #[test]
    fn a_started_session_is_tracked_then_reaped() {
        // /bin/echo ignores the arguments and exits, which is all this needs:
        // the point is that the child is tracked and later collected rather
        // than left as a zombie.
        let mut sessions = Sessions::default();
        sessions
            .start_with(&PathBuf::from("/bin/echo"), &profile())
            .unwrap();
        assert_eq!(sessions.running.len(), 1);

        // Poll until it exits; it is a trivial process, so this is quick.
        for _ in 0..200 {
            if sessions.count() == 0 {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the session was never reaped");
    }

    #[cfg(unix)]
    #[test]
    fn several_sessions_can_run_at_once() {
        let mut sessions = Sessions::default();
        for _ in 0..3 {
            sessions
                .start_with(&PathBuf::from("/bin/echo"), &profile())
                .unwrap();
        }
        assert_eq!(sessions.running.len(), 3);
    }

    #[test]
    fn a_missing_program_is_reported_rather_than_panicking() {
        let mut sessions = Sessions::default();
        let err = sessions
            .start_with(&PathBuf::from("/nonexistent/lynxrdp"), &profile())
            .unwrap_err();
        assert!(err.to_string().contains("starting"), "{err}");
        assert!(sessions.running.is_empty());
    }
}
