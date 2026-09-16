//! Helpers shared by the integration suites.
//!
//! Cargo builds every file directly under `tests/` as its own crate, so
//! anything more than one of them needs has to live in a module each declares.
//! Only the helpers a given suite actually calls are reachable from it, hence
//! the blanket `dead_code` allowance: `clippy -D warnings` would otherwise fail
//! on whichever suite happens not to use one.
#![allow(dead_code)]

use std::ops::{Deref, DerefMut};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Whether `prog` can be run.
///
/// `sshd` lives in `/usr/sbin`, which is not on a non-root `PATH` on Debian
/// derivatives, so look there as well as on `PATH`.
pub fn have(prog: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {prog} || test -x /usr/sbin/{prog}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether a FUSE filesystem can actually be mounted here.
///
/// The clipboard staging directory is a FUSE mount
/// (`lynxrdp_filecopy::Files::new`), and `fuser` wants two unrelated things for
/// it: it opens `/dev/fuse` and tries `mount(2)` directly, falling back to the
/// setuid `fusermount3` helper when the kernel refuses -- which, unprivileged,
/// it always does. Neither piece implies the other, so check both. A container
/// image can carry the `fuse3` package while its device cgroup denies the node,
/// and a host can expose the node with nothing having installed the helper.
/// Opening the device rather than stat-ing it is the point: a device that will
/// not open is an error `fuser` returns rather than retries through the helper,
/// and the interesting failure is a node that is present and not ours to use.
/// The session under test is our own child at our own uid, so what this process
/// may open, it may open too.
pub fn have_fuse() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok()
        && have("fusermount3")
}

/// Decide whether to skip a test whose external dependency is missing, and
/// return whether the caller should bail out.
///
/// A guard that prints a message and returns is reported by cargo as a
/// *passing* test. That is the right behaviour on a developer's machine, where
/// not everyone has `Xvfb` and `xclip`, and exactly the wrong one in CI: remove
/// `xvfb` from the workflow's apt line and the whole end-to-end suite goes
/// green having covered nothing whatsoever. Nothing in the output
/// distinguishes that from a real run.
///
/// `LYNXRDP_REQUIRE_E2E` is set on precisely the CI steps that install these
/// dependencies, so there an absent one is the bug it actually is rather than a
/// silent hole in the suite.
#[must_use]
pub fn skip_unless(available: bool, what: &str) -> bool {
    if available {
        return false;
    }
    assert!(
        std::env::var_os("LYNXRDP_REQUIRE_E2E").is_none(),
        "{what} -- but LYNXRDP_REQUIRE_E2E is set, so this environment is \
         supposed to be able to run the whole suite. Install the dependency or \
         unset the variable; do not let the test pass by skipping."
    );
    eprintln!("SKIP: {what}");
    true
}

/// A child process that is stopped when this handle is dropped.
///
/// `std::process::Child` does nothing on drop, so a test that panics between
/// spawning a helper and reaching the code that would have stopped it leaves
/// the helper running past the end of the cargo run: a `lynxrdp-session` with
/// its Xvfb, an `sshd -D` in a directory that has already been deleted, a
/// shell loop repainting a root window that no longer exists. Every spawn in
/// these suites goes straight into one of these, before any assertion, so an
/// unwinding test takes its helpers with it.
///
/// SIGTERM first, because the processes under test clean up on it -- the
/// session removes its authority file and runtime directory, the daemon stops
/// its sessions -- and SIGKILL only for one that has not gone after `grace`.
pub struct ChildGuard {
    child: Child,
    grace: Duration,
}

impl ChildGuard {
    /// Long enough for a session to stop its X server and desktop.
    pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

    pub fn new(child: Child) -> Self {
        Self::with_grace(child, Self::DEFAULT_GRACE)
    }

    pub fn with_grace(child: Child, grace: Duration) -> Self {
        Self { child, grace }
    }

    /// Poll for the child to exit on its own, for up to `timeout`.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(st)) = self.child.try_wait() {
                return Some(st);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop the child: SIGTERM, `grace` to act on it, then SIGKILL.
    ///
    /// A child that has already been reaped is not signalled again. Its pid
    /// is free once it is reaped and may belong to something else by the time
    /// a second call comes -- `Drop` after an explicit stop is the usual
    /// second caller -- and `Child` keeps answering `try_wait` from the status
    /// it cached, so this is exactly the check that stays safe.
    pub fn terminate(&mut self) -> Option<ExitStatus> {
        if let Ok(Some(st)) = self.child.try_wait() {
            return Some(st);
        }
        // SAFETY: signalling our own, unreaped child.
        unsafe {
            libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM);
        }
        if let Some(st) = self.wait_exit(self.grace) {
            return Some(st);
        }
        let _ = self.child.kill();
        self.child.wait().ok()
    }
}

impl Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}
