//! Helpers shared by the integration suites.
//!
//! Cargo builds every file directly under `tests/` as its own crate, so
//! anything more than one of them needs has to live in a module each declares.
//! Only the helpers a given suite actually calls are reachable from it, hence
//! the blanket `dead_code` allowance: `clippy -D warnings` would otherwise fail
//! on whichever suite happens not to use one.
#![allow(dead_code)]

use std::process::{Command, Stdio};

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

/// Decide whether to skip a test whose external dependency is missing, and
/// return whether the caller should bail out.
///
/// A guard that prints a message and returns is reported by cargo as a
/// *passing* test. That is the right behaviour on a developer's machine, where
/// not everyone has `Xvfb` and `xclip`, and exactly the wrong one in CI: remove
/// `xvfb` from the workflow's apt line and twenty-two end-to-end tests go green
/// having covered nothing whatsoever. Nothing in the output distinguishes that
/// from a real run.
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
