//! Bounded accessibility lookup for Nautilus's non-droppable empty-state overlay.
use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) struct Probe {
    pub pid: u32,
    pub x: u16,
    pub y: u16,
    pub window: (i16, i16, u16, u16),
}
impl Probe {
    pub fn start(self) -> crossbeam_channel::Receiver<Option<(u16, u16)>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = thread::Builder::new()
            .name("empty-folder-drop".into())
            .spawn(move || {
                let result = self.lookup();
                let _ = tx.send(result);
            });
        rx
    }
    fn lookup(self) -> Option<(u16, u16)> {
        let mut command = Command::new("python3");
        command
            .args(["-c", include_str!("empty_drop.py")])
            .args([
                self.pid.to_string(),
                self.x.to_string(),
                self.y.to_string(),
                self.window.0.to_string(),
                self.window.1.to_string(),
                self.window.2.to_string(),
                self.window.3.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            // Standard per-user GNOME session bus. The lookup additionally
            // matches the X window's process ID and exact frame geometry.
            command.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path=/run/user/{}/bus", unsafe { libc::geteuid() }),
            );
        }
        let mut child = command.spawn().ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    let output = child.wait_with_output().ok()?;
                    return serde_json::from_slice(&output.stdout).ok().flatten();
                }
                Ok(Some(_)) => return None,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{io, os::unix::process::ExitStatusExt, process::ExitStatus};

    /// Whether a fixture run that ended in `outcome` is a missing dependency
    /// this environment is allowed to pass over.
    ///
    /// Only a launch failure counts: the interpreter never ran, which on a
    /// machine without `python3` is an absent optional tool and nothing more. A
    /// status the fixture actually returned is its verdict on the contract, and
    /// excusing a failing one as an environment problem would hide exactly the
    /// regression the fixture exists to catch.
    ///
    /// `tests/common::skip_unless` is the shared form of this guard and argues
    /// the rest of it at length. It is out of reach here -- that module is
    /// compiled into each integration suite under `tests/`, and this is a unit
    /// test inside `src/` -- and moving the test away from the code it covers
    /// to borrow three lines would be the worse trade, so the rule it states is
    /// repeated instead: cargo reports a test that returns as a test that
    /// *passed*, so a guard that only ever skips would let a machine without
    /// `python3` report this contract as covered. `LYNXRDP_REQUIRE_E2E` marks
    /// the environments that are supposed to have the dependency, and there its
    /// absence has to be the failure it really is.
    fn skippable(outcome: &io::Result<ExitStatus>, require_e2e: bool) -> bool {
        outcome.is_err() && !require_e2e
    }

    #[test]
    fn empty_view_target_contract() {
        let outcome = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/test_empty_drop.py"
            ))
            .status();
        let require_e2e = std::env::var_os("LYNXRDP_REQUIRE_E2E").is_some();
        if skippable(&outcome, require_e2e) {
            // Report the cause rather than assuming it is a plain absence: a
            // spawn that failed for some other reason should not read as
            // "python3 is not installed" in a log.
            eprintln!("SKIP: python3 could not be run ({})", outcome.unwrap_err());
            return;
        }
        let status = outcome.expect(
            "python3 is required for the empty-view contract tests, and \
             LYNXRDP_REQUIRE_E2E is set, so this environment is supposed to be \
             able to run the whole suite. Install python3 or unset the \
             variable; do not let the test pass by skipping",
        );
        assert!(status.success(), "the empty-view contract fixture failed");
    }

    #[test]
    fn an_absent_python_skips_unless_the_environment_promised_one() {
        let absent = Err(io::Error::from(io::ErrorKind::NotFound));
        assert!(skippable(&absent, false));
        assert!(!skippable(&absent, true));
    }

    #[test]
    fn a_fixture_that_ran_and_failed_is_never_skipped() {
        // A raw wait status of 1 << 8 is exit code 1: the fixture ran and
        // reported the contract broken.
        let failed = Ok(ExitStatus::from_raw(1 << 8));
        assert!(!skippable(&failed, false));
        assert!(!skippable(&failed, true));
    }
}
