//! The privilege drop, executed for real.
//!
//! `lynxrdpd --supervise` is the only place in LynxRDP that changes
//! credentials, and the whole of it sits behind `need_switch && getuid() == 0`
//! in `daemon/supervisor.rs`. Every other suite runs the daemon with
//! `--allow-non-root`, which makes that condition permanently false, so
//! `setgroups`, `setgid`, `setuid` and the `setuid(0)` re-check -- the
//! security core of the three-process design -- are executed by no test at
//! all. This file executes them.
//!
//! # Running it
//!
//! It needs real root and is deliberately not part of the CI test steps.
//! Build it as yourself and run only the *binary* under sudo:
//!
//! ```text
//! cargo test -p lynxrdp-server --test privdrop --no-run
//! sudo ./target/debug/deps/privdrop-<hash> --test-threads=1 --nocapture
//! ```
//!
//! `sudo cargo test` is the wrong shape: cargo would write to `target/` as
//! root and leave a tree the ordinary user can no longer build in, which
//! outlives the test run and is tedious to undo.
//!
//! Without root the credential tests skip, via the shared `skip_unless` guard,
//! so a plain `cargo test --workspace` stays green. That guard turns a skip
//! into a failure when `LYNXRDP_REQUIRE_E2E` is set, which is what stops this
//! file from quietly passing having proved nothing; do not add that variable
//! to a step that runs this suite as an ordinary user.
//!
//! # What it proves
//!
//! `/proc/<pid>/status` of the process the supervisor exec'd, read by that
//! process itself. `Uid:` and `Gid:` each carry four values -- real,
//! effective, saved and filesystem -- and the *saved* one is the proof that
//! matters: a process whose saved uid is still 0 may call `setuid(0)` and be
//! root again, so a half-done drop looks identical in `id` output and is worth
//! nothing. `Groups:` proves `setgroups` ran; without it the session would
//! inherit root's supplementary groups and keep read access to everything
//! group 0 can reach, while looking perfectly unprivileged.

use std::ffi::{CStr, CString};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lynxrdp_server::daemon::users::UserInfo;

mod common;
use common::skip_unless;

macro_rules! require_root {
    () => {
        if skip_unless(
            lynxrdp_server::peer::own_uid() == 0,
            "not running as root, so no privilege can be dropped (see this \
             file's header for how to run it under sudo)",
        ) {
            return;
        }
    };
}

/// The credential lines of a `/proc/<pid>/status`.
#[derive(Debug, PartialEq, Eq)]
struct Credentials {
    /// Real, effective, saved and filesystem uid, in that order.
    uid: [u32; 4],
    /// Real, effective, saved and filesystem gid, in that order.
    gid: [u32; 4],
    /// Supplementary groups, exactly as the kernel printed them.
    groups: Vec<u32>,
}

/// Pull `Uid:`, `Gid:` and `Groups:` out of a `/proc/<pid>/status`.
///
/// Strict on purpose. A three-field `Uid:` line, or an absent one, means the
/// text is not what we think it is, and reading a missing value as zero here
/// would turn "the drop did not happen" into "the drop looks fine".
///
/// Pure, so it is the one part of this file a machine with no root can check.
fn parse_credentials(status: &str) -> Result<Credentials, String> {
    fn ids(rest: &str) -> Result<Vec<u32>, String> {
        rest.split_whitespace()
            .map(|f| f.parse::<u32>().map_err(|_| format!("{f:?} is not an id")))
            .collect()
    }
    fn four(what: &str, rest: &str) -> Result<[u32; 4], String> {
        let v = ids(rest)?;
        <[u32; 4]>::try_from(v.as_slice())
            .map_err(|_| format!("{what} has {} fields, expected 4", v.len()))
    }
    let mut uid = None;
    let mut gid = None;
    let mut groups = None;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            uid = Some(four("Uid:", rest)?);
        } else if let Some(rest) = line.strip_prefix("Gid:") {
            gid = Some(four("Gid:", rest)?);
        } else if let Some(rest) = line.strip_prefix("Groups:") {
            groups = Some(ids(rest)?);
        }
    }
    Ok(Credentials {
        uid: uid.ok_or("no Uid: line")?,
        gid: gid.ok_or("no Gid: line")?,
        groups: groups.ok_or("no Groups: line")?,
    })
}

/// Sort and deduplicate a group list before comparing two of them.
///
/// The kernel sorts what `setgroups` was handed, and `getgrouplist` may return
/// the primary gid twice when the account is also listed against it in the
/// group file. Neither is a difference worth failing on.
fn normalize(mut gids: Vec<u32>) -> Vec<u32> {
    gids.sort_unstable();
    gids.dedup();
    gids
}

/// Look up an account by name.
///
/// `daemon::users` offers only a uid lookup, and this suite has to *name* the
/// account it drops to -- the group lookup takes a name, not a number.
fn user_by_name(name: &str) -> Option<UserInfo> {
    let c_name = CString::new(name).ok()?;
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: getpwnam_r writes into buffers we own, and every string is
    // copied out before those buffers are dropped.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwnam_r(
            c_name.as_ptr(),
            &mut pw,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        );
        if rc != 0 || result.is_null() {
            return None;
        }
        Some(UserInfo {
            uid: pw.pw_uid,
            gid: pw.pw_gid,
            name: CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned(),
            home: CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned(),
            shell: CStr::from_ptr(pw.pw_shell).to_string_lossy().into_owned(),
        })
    }
}

/// An unprivileged account to drop to.
///
/// `nobody` first: it exists on every distribution we package for, owns
/// nothing and is in no interesting group. The rest are fallbacks for a
/// stripped container. `LYNXRDP_TEST_USER` overrides the choice for a host
/// where none of them is suitable.
fn target_user() -> Option<UserInfo> {
    if let Ok(name) = std::env::var("LYNXRDP_TEST_USER") {
        // Naming an account that does not exist is a typo, and falling back to
        // the list below would then drop to some other account and report a
        // pass for a run that tested something the operator did not ask for.
        return Some(user_by_name(&name).unwrap_or_else(|| {
            panic!("LYNXRDP_TEST_USER names {name:?}, and this host has no such account")
        }));
    }
    ["nobody", "nfsnobody", "daemon", "bin", "games"]
        .into_iter()
        .filter_map(user_by_name)
        .find(|u| u.uid != 0 && u.gid != 0)
}

/// The supplementary groups the session should end up with.
///
/// Deliberately the same resolver the supervisor uses. Reimplementing
/// `getgrouplist` here would be a second copy of the lookup, free to drift, and
/// it is not the lookup that is untested: `group_ids` has unit tests of its
/// own, while the `setgroups` call that installs its answer in a process that
/// is about to stop being root has none. What this pins is that the answer
/// reached the child at all -- an omitted `setgroups` leaves root's own list in
/// place, which no amount of correct resolution would show up.
fn expected_groups(user: &UserInfo) -> Vec<u32> {
    let gids = lynxrdp_server::daemon::users::group_ids(&user.name, user.gid)
        .unwrap_or_else(|e| panic!("resolving the groups of {}: {e:#}", user.name));
    normalize(gids)
}

fn wait_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(st)) = child.try_wait() {
            return Some(st);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// Run `lynxrdpd --supervise` for `user` with a probe in place of
/// `lynxrdp-session`, and return the credentials the exec'd process saw of
/// itself along with the owner of the file it wrote.
///
/// The real session binary would prove nothing extra and would need an X
/// server: `pre_exec` has already finished by the time anything is exec'd, so
/// whatever runs sees the final credentials. `/bin/sh -c` rather than a script
/// dropped in the temporary directory, because `/tmp` is mounted `noexec` on
/// hardened hosts and a script there would fail to exec for a reason that has
/// nothing to do with what is being tested.
fn supervised_credentials(user: &UserInfo) -> (Credentials, u32) {
    let dir = tempfile::tempdir().expect("tempdir");
    // Root created it 0700; the probe runs as somebody else and has to be able
    // to create its output file in here.
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
        .expect("opening up the temporary directory");
    let status_path = dir.path().join("status");
    // $$ is the shell itself -- the process the supervisor exec'd -- rather
    // than the `cat` it forks, though the two have the same credentials.
    let script = format!("cat /proc/$$/status > \"{}\"", status_path.display());

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_lynxrdpd"));
    cmd.arg("--supervise")
        .arg("--uid")
        .arg(user.uid.to_string())
        .arg("--gid")
        .arg(user.gid.to_string())
        .arg("--user")
        .arg(&user.name)
        .arg("--home")
        .arg(&user.home)
        .arg("--shell")
        .arg("/bin/sh")
        // Any open descriptor will do. The supervisor only duplicates these
        // into place for the child (log onto 1 and 2, control onto 3) and the
        // probe reads neither, so passing stdin and stdout saves the test
        // inventing a socket purely to be inherited.
        .arg("--control-fd")
        .arg("0")
        .arg("--log-fd")
        .arg("1")
        .arg("--session-binary")
        .arg("/bin/sh")
        // Everything after `--` is the session's own argument list, which is
        // how the daemon calls it too. The supervisor appends --control-fd and
        // --username, which land in $1.. and are ignored.
        .arg("--")
        .arg("-c")
        .arg(&script)
        .arg("lynxrdp-privdrop-probe")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env("RUST_LOG", "debug");
    let mut child = cmd.spawn().expect("start lynxrdpd --supervise");
    let Some(status) = wait_exit(&mut child, Duration::from_secs(30)) else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the supervisor never exited");
    };
    assert!(
        status.success(),
        "the supervisor exited with {status}; its log is above"
    );

    let text = std::fs::read_to_string(&status_path).unwrap_or_else(|e| {
        panic!(
            "the probe wrote no status file ({e}); the supervisor exited {status}, \
             so the failure is in pre_exec -- setgroups or setgid or setuid"
        )
    });
    let owner = std::fs::metadata(&status_path)
        .expect("stat the status file")
        .uid();
    let creds = parse_credentials(&text).unwrap_or_else(|e| panic!("{e} in /proc status:\n{text}"));
    (creds, owner)
}

#[test]
fn credential_lines_are_parsed_as_the_kernel_writes_them() {
    let dropped = "Name:\tsh\n\
         Uid:\t65534\t65534\t65534\t65534\n\
         Gid:\t65534\t65534\t65534\t65534\n\
         Groups:\t65534 \n\
         CapEff:\t0000000000000000\n";
    let c = parse_credentials(dropped).unwrap();
    assert_eq!(c.uid, [65534; 4]);
    assert_eq!(c.gid, [65534; 4]);
    assert_eq!(c.groups, vec![65534]);

    // What the same lines look like when the drop did not happen. Telling
    // these two apart is the entire point of the tests below.
    let still_root = "Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nGroups:\t0\n";
    assert_eq!(parse_credentials(still_root).unwrap().uid, [0; 4]);

    // And the shape that would be worst to misread: dropped everywhere except
    // the saved uid, from which root is one setuid(0) away.
    let half = "Uid:\t65534\t65534\t0\t65534\nGid:\t65534\t65534\t65534\t65534\nGroups:\t0\n";
    assert_eq!(
        parse_credentials(half).unwrap().uid,
        [65534, 65534, 0, 65534]
    );

    // An empty supplementary list is legal and is not an absent line.
    let no_groups = "Uid:\t1\t1\t1\t1\nGid:\t1\t1\t1\t1\nGroups:\t\n";
    assert!(parse_credentials(no_groups).unwrap().groups.is_empty());

    // Missing or malformed lines are errors, never silent zeroes.
    assert!(parse_credentials("Gid:\t0\t0\t0\t0\nGroups:\t0\n").is_err());
    assert!(parse_credentials("Uid:\t0\t0\t0\t0\nGroups:\t0\n").is_err());
    assert!(parse_credentials("Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\n").is_err());
    assert!(parse_credentials("Uid:\t0\t0\t0\nGid:\t0\t0\t0\t0\nGroups:\n").is_err());
    assert!(parse_credentials("Uid:\tx\t0\t0\t0\nGid:\t0\t0\t0\t0\nGroups:\n").is_err());
}

#[test]
fn group_lists_compare_regardless_of_order_or_repeats() {
    assert_eq!(normalize(vec![9, 1, 9, 4]), vec![1, 4, 9]);
    assert!(normalize(Vec::new()).is_empty());
}

#[test]
fn the_session_runs_with_the_target_users_credentials() {
    require_root!();
    let Some(user) = target_user() else {
        panic!("no unprivileged account to drop to; set LYNXRDP_TEST_USER");
    };
    let want_groups = expected_groups(&user);
    assert!(
        !want_groups.contains(&0),
        "{} is a member of group 0, so dropping to it proves nothing; pick \
         another account with LYNXRDP_TEST_USER",
        user.name
    );

    let (creds, owner) = supervised_credentials(&user);

    // The saved uid first, because it is the one an incomplete drop leaves
    // behind and the one nothing else would reveal. setuid() as root sets all
    // three, which is why supervisor.rs uses it rather than seteuid(); the
    // setuid(0) re-check that follows is belt and braces for exactly this.
    assert_eq!(
        creds.uid[2], user.uid,
        "saved uid is {} rather than {}: the session could call setuid(0) and \
         be root again",
        creds.uid[2], user.uid
    );
    assert_eq!(
        creds.uid, [user.uid; 4],
        "real/effective/saved/fs uid should all be {} for {}",
        user.uid, user.name
    );
    assert_eq!(
        creds.gid, [user.gid; 4],
        "real/effective/saved/fs gid should all be {} for {}",
        user.gid, user.name
    );

    assert_eq!(
        normalize(creds.groups.clone()),
        want_groups,
        "supplementary groups are wrong: setgroups() either did not run or \
         ran for the wrong account (kernel said {:?})",
        creds.groups
    );
    // Stated separately because it is the consequence, not the mechanism: a
    // process that kept group 0 reads root's files whatever its uid says.
    assert!(
        !creds.groups.contains(&0),
        "root's supplementary group survived the drop: {:?}",
        creds.groups
    );

    // The filesystem uid, demonstrated rather than read off a line: the file
    // the probe created belongs to the target account.
    assert_eq!(
        owner, user.uid,
        "the session wrote a file owned by uid {owner}, not {}",
        user.uid
    );
}

#[test]
fn serving_root_itself_changes_no_credentials() {
    require_root!();
    // `need_switch` is false when the target *is* the invoking uid, and the
    // drop must then be skipped rather than attempted: a `setuid(0)` from root
    // succeeds, so a condition that stopped distinguishing the two cases would
    // sail through the re-check below it and only be noticed the first time a
    // real user connected. Serving root is a supported configuration -- the
    // daemon does it whenever root opens a session -- so it is testable here.
    let root = user_by_name("root").expect("root account");
    assert_eq!(root.uid, 0, "the root account is not uid 0");
    // Our own supplementary list, read exactly the way the child's is read.
    // Nothing on this path calls setgroups, so the child inherits ours
    // unchanged, and saying so is what makes this a claim about *credentials*
    // rather than about two uid lines: hoisting the setgroups call out from
    // under `need_switch` would hand it the empty list the no-switch branch
    // prepares, stripping root's session of every group while `Uid:` and
    // `Gid:` still read 0 0 0 0 and this test still passed.
    let own = std::fs::read_to_string("/proc/self/status").expect("our own /proc status");
    let own = parse_credentials(&own).unwrap_or_else(|e| panic!("{e} in our own /proc status"));
    let (creds, owner) = supervised_credentials(&root);
    assert_eq!(creds.uid, [0; 4], "root's session should still be root");
    assert_eq!(creds.gid, [0; 4], "root's session should still be gid 0");
    assert_eq!(
        normalize(creds.groups.clone()),
        normalize(own.groups),
        "the supplementary groups changed on a path that switches nothing: \
         the child has {:?}",
        creds.groups
    );
    assert_eq!(owner, 0);
}
