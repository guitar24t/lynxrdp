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
//!
//! The price of the split is that everything a session has to say about a
//! failed connection -- "Permission denied (publickey)", "Host key
//! verification failed", a policy rejection from the server -- is written to
//! the child's standard error and would otherwise be thrown away, leaving a
//! click on Connect looking exactly like a click on nothing. So the child's
//! stderr is kept and its exit status is read; see [`Sessions::reap`].

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};

use anyhow::{Context, Result};

use crate::profiles::Profile;

/// How much of a session's stderr is looked at when it fails.
///
/// The interesting part of an SSH failure is always the last few lines; a
/// session that ran for an hour and logged steadily could have megabytes
/// before them.
const TAIL_BYTES: u64 = 4096;

/// How many of those lines are shown.
const TAIL_LINES: usize = 4;

/// Longest message we build from them, so one absurd line cannot push the
/// rest of the launcher's window off the screen.
const MAX_DETAIL: usize = 600;

/// Failures kept while waiting to be shown. A launcher drains this every
/// repaint, so the bound only matters if one somehow stops asking.
const MAX_PENDING: usize = 16;

/// A session this launcher started.
struct Session {
    child: Child,
    /// Profile name, which is what the user recognises in the list.
    name: String,
    /// `user@host`, to disambiguate two profiles with similar names.
    destination: String,
    /// The child's standard error, as a file rather than a pipe.
    ///
    /// A pipe would be the obvious choice and is the wrong one: nothing here
    /// reads it until the child exits, so the child would wedge forever the
    /// moment it wrote past the pipe buffer -- 64 KiB on Linux, less
    /// elsewhere. Draining it would mean a reader thread per session. An
    /// unlinked temporary file has neither problem: the kernel absorbs
    /// everything, and the file disappears when the last handle closes.
    log: File,
}

/// A session that exited without being asked to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFailure {
    /// Profile name.
    pub name: String,
    /// `user@host`.
    pub destination: String,
    /// How it ended, in words: "exit code 255", "killed by signal 11".
    pub status: String,
    /// The tail of its standard error, if it said anything.
    pub detail: Option<String>,
}

impl SessionFailure {
    /// What the launcher shows.
    pub fn message(&self) -> String {
        let Self {
            name,
            destination,
            status,
            detail,
        } = self;
        match detail {
            Some(detail) => format!("{name} ({destination}) ended -- {status}:\n{detail}"),
            // Worth saying explicitly. Silence here means the session died
            // without diagnosing itself, which points at the session binary
            // rather than at the connection.
            None => format!(
                "{name} ({destination}) ended -- {status}, with nothing on its error output."
            ),
        }
    }
}

/// Sessions started by this launcher, kept so they can be reaped.
///
/// A child that is never waited for stays a zombie on Unix, so something has
/// to collect them regardless; collecting the exit status too is what turns a
/// silent failure into a message.
#[derive(Debug, Default)]
pub struct Sessions {
    running: Vec<Session>,
    /// Failures noticed by [`Self::reap`] and not yet shown.
    ///
    /// Buffered rather than returned because `reap` runs more than once per
    /// repaint -- the launcher calls it directly and again through
    /// [`Self::count`] -- and a returned value would be seen by whichever
    /// call happened to notice the exit and lost by the other.
    finished: Vec<SessionFailure>,
}

// Hand-written rather than derived so that a `{:?}` of the launcher's state
// says which session is which, instead of a raw file handle and a Child's
// platform internals.
impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("name", &self.name)
            .field("destination", &self.destination)
            .field("pid", &self.child.id())
            .finish()
    }
}

impl Sessions {
    /// Start `profile` by re-invoking this executable.
    pub fn start(&mut self, profile: &Profile) -> Result<()> {
        let program = std::env::current_exe().context("finding this executable")?;
        self.start_with(&program, profile)
    }

    /// Start using an explicit program, which is what the tests use.
    pub fn start_with(&mut self, program: &Path, profile: &Profile) -> Result<()> {
        self.spawn(
            program,
            profile.args(),
            &profile.name,
            &profile.destination(),
        )
    }

    /// Spawn one session and start tracking it.
    fn spawn(
        &mut self,
        program: &Path,
        args: Vec<String>,
        name: &str,
        destination: &str,
    ) -> Result<()> {
        let log = tempfile::tempfile().context("creating a file for the session's messages")?;
        // Two handles onto one file: the child writes through its own, we
        // read back through ours after it exits.
        let for_child = log
            .try_clone()
            .context("duplicating the session's message file")?;
        let mut command = Command::new(program);
        command
            .args(args)
            // The session draws a window and has no console, so there is
            // nowhere for ssh to ask about an unknown host key or a key
            // passphrase; started from a desktop it has no controlling
            // terminal either, so it refuses rather than waits. That
            // refusal goes to stderr, which is the reason stderr is kept.
            .stdin(Stdio::null())
            // The session draws its own window and prints nothing useful on
            // stdout; inheriting it would keep a terminal busy.
            .stdout(Stdio::null())
            .stderr(Stdio::from(for_child));
        no_console_window(&mut command);
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        self.running.push(Session {
            child,
            name: name.to_string(),
            destination: destination.to_string(),
            log,
        });
        Ok(())
    }

    /// Collect any sessions that have exited, remembering the failures.
    ///
    /// Called from the launcher's repaint, which is frequent and must not
    /// block, so this only ever polls.
    pub fn reap(&mut self) {
        let mut still_running = Vec::with_capacity(self.running.len());
        for mut session in std::mem::take(&mut self.running) {
            match session.child.try_wait() {
                Ok(None) => still_running.push(session),
                // A clean exit is the user closing the session window.
                Ok(Some(status)) if status.success() => {}
                Ok(Some(status)) => {
                    let failure = SessionFailure {
                        name: session.name.clone(),
                        destination: session.destination.clone(),
                        status: describe(status),
                        detail: tail(&mut session.log),
                    };
                    self.remember(failure);
                }
                // Keeping it is the safe side of an error that should not
                // happen: dropping it would leak a zombie, and try_wait will
                // be tried again on the next repaint.
                Err(_) => still_running.push(session),
            }
        }
        self.running = still_running;
    }

    /// Queue a failure for the launcher to show.
    ///
    /// Bounded because nothing here can be sure anyone is draining: a
    /// launcher that stopped repainting would otherwise keep one of these
    /// per session for as long as it ran. The oldest goes first -- what the
    /// user is waiting to hear about is the session they just started.
    fn remember(&mut self, failure: SessionFailure) {
        if self.finished.len() >= MAX_PENDING {
            self.finished.remove(0);
        }
        self.finished.push(failure);
    }

    /// Take the oldest failure not yet shown, if any.
    pub fn take_failure(&mut self) -> Option<SessionFailure> {
        if self.finished.is_empty() {
            return None;
        }
        Some(self.finished.remove(0))
    }

    /// How many sessions this launcher started are still running.
    pub fn count(&mut self) -> usize {
        self.reap();
        self.running.len()
    }
}

/// An exit status in words.
///
/// `ExitStatus`'s own Display is close, but says "exit status: 255" with a
/// colon in the middle of our sentence, and on Unix a signal reads as
/// "signal: 11 (SIGSEGV)".
fn describe(status: ExitStatus) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("killed by signal {signal}");
        }
    }
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "ended abnormally".to_string(),
    }
}

/// The last few meaningful lines the session wrote to standard error.
///
/// Every step is fallible and none of it is worth an error path: this runs
/// only to explain a failure that has already happened, and a launcher that
/// reported "could not read the error log" instead of the error would be
/// worse than one that reported nothing.
fn tail(log: &mut File) -> Option<String> {
    let length = log.metadata().ok()?.len();
    let from = length.saturating_sub(TAIL_BYTES);
    log.seek(SeekFrom::Start(from)).ok()?;
    let mut raw = Vec::new();
    log.take(TAIL_BYTES).read_to_end(&mut raw).ok()?;
    // Lossy on purpose: a session's stderr carries whatever the remote host
    // and the local SSH felt like writing, and one stray byte must not cost
    // the whole message.
    let text = String::from_utf8_lossy(&raw);
    let kept = tail_lines(&text, from > 0, TAIL_LINES);
    if kept.is_empty() {
        None
    } else {
        Some(kept)
    }
}

/// Pick the last `max_lines` non-blank lines out of `text`.
///
/// `mid_file` says the text started at an arbitrary byte offset rather than
/// at the beginning of the file, in which case its first line is very
/// probably half a line and would read as noise.
fn tail_lines(text: &str, mid_file: bool, max_lines: usize) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    if mid_file && !lines.is_empty() {
        lines.remove(0);
    }
    let mut kept: Vec<&str> = lines
        .iter()
        .rev()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .take(max_lines)
        .collect();
    kept.reverse();
    let joined = kept.join("\n");
    if joined.len() <= MAX_DETAIL {
        return joined;
    }
    // Cut from the front: the last line is the one that says why.
    let start = joined.len() - MAX_DETAIL;
    let start = (start..joined.len())
        .find(|i| joined.is_char_boundary(*i))
        .unwrap_or(joined.len());
    format!("...{}", &joined[start..])
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

    /// Run a shell snippet as if it were a session, and wait for it.
    ///
    /// /bin/sh stands in for the session binary so a test can produce a
    /// specific exit status and a specific complaint on stderr, which is the
    /// whole of what this module has to get right.
    #[cfg(unix)]
    fn run_script(sessions: &mut Sessions, script: &str) {
        sessions
            .spawn(
                Path::new("/bin/sh"),
                vec!["-c".into(), script.into()],
                "test",
                "alice@example.org",
            )
            .unwrap();
        for _ in 0..500 {
            sessions.reap();
            if sessions.running.is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the session never exited");
    }

    #[cfg(unix)]
    #[test]
    fn a_started_session_is_tracked_then_reaped() {
        // /bin/echo ignores the arguments and exits, which is all this needs:
        // the point is that the child is tracked and later collected rather
        // than left as a zombie.
        let mut sessions = Sessions::default();
        sessions
            .start_with(Path::new("/bin/echo"), &profile())
            .unwrap();
        assert_eq!(sessions.running.len(), 1);

        // Poll until it exits; it is a trivial process, so this is quick.
        for _ in 0..200 {
            if sessions.count() == 0 {
                // A successful exit is the user closing the window, not a
                // failure to report.
                assert!(sessions.take_failure().is_none());
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
                .start_with(Path::new("/bin/echo"), &profile())
                .unwrap();
        }
        assert_eq!(sessions.running.len(), 3);
    }

    #[test]
    fn a_missing_program_is_reported_rather_than_panicking() {
        let mut sessions = Sessions::default();
        let err = sessions
            .start_with(Path::new("/nonexistent/lynxrdp"), &profile())
            .unwrap_err();
        assert!(err.to_string().contains("starting"), "{err}");
        assert!(sessions.running.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_connection_keeps_what_ssh_said() {
        // The failure this whole module exists for: ssh refuses the key and
        // says so on stderr, and the launcher must be able to repeat it.
        let mut sessions = Sessions::default();
        run_script(
            &mut sessions,
            "echo 'alice@example.org: Permission denied (publickey).' >&2; exit 255",
        );
        let failure = sessions.take_failure().expect("the failure was not kept");
        assert_eq!(failure.status, "exit code 255");
        assert!(
            failure.detail.as_deref().unwrap().contains("publickey"),
            "{failure:?}"
        );
        let message = failure.message();
        assert!(message.contains("alice@example.org"), "{message}");
        assert!(sessions.take_failure().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_failure_survives_being_reaped_repeatedly() {
        // reap() runs twice per repaint -- once directly and once through
        // count() -- so a failure that were returned rather than buffered
        // would be lost by whichever call did not see the exit.
        let mut sessions = Sessions::default();
        run_script(&mut sessions, "echo boom >&2; exit 1");
        for _ in 0..5 {
            sessions.reap();
            assert_eq!(sessions.count(), 0);
        }
        let failure = sessions
            .take_failure()
            .expect("the failure was reaped away");
        assert_eq!(failure.detail.as_deref(), Some("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn a_session_that_says_nothing_still_reports_its_status() {
        let mut sessions = Sessions::default();
        run_script(&mut sessions, "exit 3");
        let failure = sessions.take_failure().unwrap();
        assert_eq!(failure.detail, None);
        assert!(failure.message().contains("exit code 3"), "{failure:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_signalled_session_is_described_as_such() {
        let mut sessions = Sessions::default();
        // 128+9 is what the shell reports, but the status we read is the
        // real one, so this must not come out as "exit code 137".
        run_script(&mut sessions, "kill -9 $$");
        let failure = sessions.take_failure().unwrap();
        assert_eq!(failure.status, "killed by signal 9");
    }

    #[cfg(unix)]
    #[test]
    fn a_chatty_session_does_not_wedge_on_a_full_pipe() {
        // The reason stderr is a file: a pipe nobody drains blocks the child
        // forever once the buffer fills, which is 64 KiB on Linux. A megabyte
        // is comfortably past every platform's buffer.
        let mut sessions = Sessions::default();
        run_script(
            &mut sessions,
            "i=0; while [ $i -lt 20000 ]; do echo 'chatter chatter chatter chatter chatter chatter' >&2; i=$((i+1)); done; echo 'the last word' >&2; exit 1",
        );
        let failure = sessions.take_failure().unwrap();
        // The tail must be the end of the output, not the beginning.
        assert!(
            failure
                .detail
                .as_deref()
                .unwrap()
                .ends_with("the last word"),
            "{failure:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn several_failures_queue_up_in_order() {
        let mut sessions = Sessions::default();
        run_script(&mut sessions, "echo first >&2; exit 1");
        run_script(&mut sessions, "echo second >&2; exit 1");
        assert_eq!(
            sessions.take_failure().unwrap().detail.as_deref(),
            Some("first")
        );
        assert_eq!(
            sessions.take_failure().unwrap().detail.as_deref(),
            Some("second")
        );
        assert!(sessions.take_failure().is_none());
    }

    #[test]
    fn the_queue_of_failures_is_bounded() {
        // Nothing forces a launcher to keep draining, and a session that
        // reconnects in a loop would otherwise grow this without limit.
        let mut sessions = Sessions::default();
        for n in 0..MAX_PENDING + 3 {
            sessions.remember(SessionFailure {
                name: format!("session {n}"),
                destination: "alice@example.org".into(),
                status: "exit code 1".into(),
                detail: None,
            });
        }
        assert_eq!(sessions.finished.len(), MAX_PENDING);
        // The oldest three went, and what is left is still in order.
        assert_eq!(sessions.take_failure().unwrap().name, "session 3");
        assert_eq!(sessions.take_failure().unwrap().name, "session 4");
    }

    #[test]
    fn the_tail_keeps_the_last_lines_in_order() {
        let text = "one\ntwo\nthree\nfour\nfive\n";
        assert_eq!(tail_lines(text, false, 3), "three\nfour\nfive");
    }

    #[test]
    fn the_tail_skips_blank_lines() {
        // ssh separates its banners with blank lines; four "lines" of nothing
        // would push the real message out of a four-line tail.
        let text = "Host key verification failed.\n\n\n\n";
        assert_eq!(tail_lines(text, false, 4), "Host key verification failed.");
    }

    #[test]
    fn the_tail_drops_a_half_line_at_the_start() {
        // Reading the last N bytes almost always lands mid-line, and half a
        // sentence at the top reads as corruption.
        assert_eq!(tail_lines("denied (publickey).\nbye\n", true, 4), "bye");
        // Unless the text really did start at the beginning of the file.
        assert_eq!(
            tail_lines("denied (publickey).\nbye\n", false, 4),
            "denied (publickey).\nbye"
        );
    }

    #[test]
    fn the_tail_of_a_single_partial_line_is_empty_rather_than_wrong() {
        assert_eq!(tail_lines("half a li", true, 4), "");
    }

    #[test]
    fn the_tail_is_bounded_even_for_one_enormous_line() {
        let text = "x".repeat(MAX_DETAIL * 3);
        let kept = tail_lines(&text, false, 4);
        assert!(kept.len() <= MAX_DETAIL + 3, "{}", kept.len());
        assert!(kept.starts_with("..."));
    }

    #[test]
    fn the_tail_cut_does_not_split_a_character() {
        // Session output is arbitrary text from a remote host, and cutting it
        // at a fixed byte offset would panic the launcher the first time that
        // offset landed inside a multi-byte character. Sized so that it does.
        let text = format!("\u{20ac}{}", "b".repeat(MAX_DETAIL - 2));
        let kept = tail_lines(&text, false, 4);
        assert!(kept.starts_with("...b"), "{kept}");
        assert!(!kept.contains('\u{20ac}'), "a character was cut in half");
    }

    #[test]
    fn the_tail_of_nothing_is_nothing() {
        assert_eq!(tail_lines("", false, 4), "");
        assert_eq!(tail_lines("\n \n\t\n", false, 4), "");
    }
}
