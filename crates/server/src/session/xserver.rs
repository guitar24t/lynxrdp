//! Launching the headless X server (Xvfb by default).

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::xauth;

/// How many trailing stderr lines are kept for a post-mortem.
///
/// Enough to hold an X server's dying words (a fatal error is usually one line
/// preceded by a few of context) and small enough that keeping it costs
/// nothing for the whole life of a session.
const TAIL_LINES: usize = 24;

/// Longest stderr line kept, in characters, so one pathological line cannot
/// push everything useful out of the tail or out of a log message.
const MAX_LINE_CHARS: usize = 400;

/// How long the drain thread is given to reach EOF once the X server is dead.
const DRAIN_SETTLE: Duration = Duration::from_millis(500);

/// Settings for the X server process.
#[derive(Clone, Debug)]
pub struct XServerConfig {
    /// Executable (`Xvfb`).
    pub program: String,
    /// Additional arguments appended to the generated ones.
    pub extra_args: Vec<String>,
    /// Virtual screen width (the largest size a client may ask for).
    pub max_width: u32,
    /// Virtual screen height.
    pub max_height: u32,
    /// DPI to report.
    pub dpi: u32,
    /// Directory for the authority file and other private files.
    pub runtime_dir: PathBuf,
}

/// A running X server.
pub struct XServer {
    child: Child,
    display_num: u32,
    xauth_path: tempfile::TempPath,
    /// Program name, repeated here so post-mortem messages can name it.
    program: String,
    /// The last few lines the server wrote to stderr.
    stderr_tail: StderrTail,
    /// The thread filling `stderr_tail`, so it can be given a moment to
    /// finish before the tail is read.
    drain: Option<JoinHandle<()>>,
    /// Set by `shutdown`, so `Drop` does not run it a second time.
    stopped: bool,
}

impl XServer {
    /// Start the server and wait until it reports its display number.
    pub fn spawn(cfg: &XServerConfig) -> Result<Self> {
        ensure_private_dir(&cfg.runtime_dir)?;
        let cookie = xauth::random_cookie()?;
        let uid = crate::peer::own_uid();
        let user_runtime = PathBuf::from(format!("/run/user/{uid}"));
        let auth_dir = authority_dir(&cfg.runtime_dir, &user_runtime, uid)?;
        // Snap's desktop interface permits /run/user/<uid>/xauth_* but not
        // our nested runtime directory. A unique file avoids replacing a
        // console's cookie or another X server's, and TempPath retires it on
        // every startup error as well as normal shutdown. Explicit runtime
        // directories keep their authority file inside the configured path.
        let prefix = if auth_dir != cfg.runtime_dir {
            "xauth_lynxrdp-"
        } else {
            "Xauthority-"
        };
        let xauth_path = tempfile::Builder::new()
            .prefix(prefix)
            .tempfile_in(auth_dir)
            .context("creating the private Xauthority file")?
            .into_temp_path();
        // Placeholder display number; rewritten once known. The X server only
        // reads the cookie from the file, not the display number.
        xauth::write_file(&xauth_path, 0, &cookie)?;

        let mut fds = [0i32; 2];
        // SAFETY: fds is a valid 2-element array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe");
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: we own read_fd.
        let mut reader = unsafe { fs::File::from_raw_fd(read_fd) };

        let mut cmd = Command::new(&cfg.program);
        cmd.arg("-displayfd")
            .arg(write_fd.to_string())
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}x24", cfg.max_width, cfg.max_height))
            .arg("-nolisten")
            .arg("tcp")
            .arg("-noreset")
            .arg("-auth")
            .arg(&xauth_path)
            .arg("-dpi")
            .arg(cfg.dpi.to_string())
            .args([
                "+extension",
                "RANDR",
                "+extension",
                "DAMAGE",
                "+extension",
                "XFIXES",
                "+extension",
                "MIT-SHM",
                "+extension",
                "XTEST",
            ])
            .args(&cfg.extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env_remove("DISPLAY");
        // SAFETY: only async-signal-safe calls (close, setsid) in the child.
        unsafe {
            cmd.pre_exec(move || {
                libc::close(read_fd);
                libc::setsid();
                // Die with the session process so no X server is ever orphaned.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting X server {}", cfg.program))?;
        // SAFETY: we own write_fd in the parent; the child has its own copy.
        unsafe {
            libc::close(write_fd);
        }

        // Start draining stderr immediately, before we wait for the display
        // number: a server that is slow to start is often a server that is
        // saying why, and a full pipe would stop it saying anything more.
        let stderr_tail = StderrTail::new();
        let drain = child
            .stderr
            .take()
            .and_then(|se| spawn_drain(se, cfg.program.clone(), stderr_tail.clone()));

        // Xvfb writes "<n>\n" to displayfd once it is listening.
        let display_num =
            match read_display_number(&mut reader, &mut child, Duration::from_secs(20)) {
                Ok(n) => n,
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = fs::remove_file(&xauth_path);
                    // Report from this thread, not from the drain thread: the
                    // session's `main` finishes with `process::exit`, which
                    // cuts a detached thread off mid-write, and a failed start
                    // is precisely when it is about to do that.
                    let lines = collect_tail(drain.as_ref(), &stderr_tail);
                    report_tail(&cfg.program, &lines);
                    let detail = match lines.last() {
                        Some(l) => format!(": {l}"),
                        None => String::new(),
                    };
                    return Err(
                        e.context(format!("X server {} failed to start{detail}", cfg.program))
                    );
                }
            };
        xauth::write_file(&xauth_path, display_num, &cookie)?;
        log::info!("X server pid {} on display :{display_num}", child.id());
        Ok(Self {
            child,
            display_num,
            xauth_path,
            program: cfg.program.clone(),
            stderr_tail,
            drain,
            stopped: false,
        })
    }

    /// `:N` display string.
    pub fn display(&self) -> String {
        format!(":{}", self.display_num)
    }

    /// Display number.
    pub fn display_num(&self) -> u32 {
        self.display_num
    }

    /// Authority file path (export as `XAUTHORITY`).
    pub fn xauth_path(&self) -> &Path {
        &self.xauth_path
    }

    /// Process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the server process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate the server.
    ///
    /// Idempotent: `Drop` calls it again, and after the first call the child
    /// has been reaped, so signalling its pid a second time would be aimed at
    /// whatever process the kernel has since given that number to.
    pub fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // Taken before anything else, because `try_wait` reaps a child that
        // has already exited and its pid then belongs to whoever the kernel
        // hands it to next -- not something to send SIGTERM to.
        let already_exited = self.child.try_wait().ok().flatten();
        match already_exited {
            // It died on its own rather than at our request, which is the
            // case an administrator needs explained.
            Some(status) => {
                log::error!("X server {} exited on its own: {status}", self.program);
                let lines = collect_tail(self.drain.as_ref(), &self.stderr_tail);
                report_tail(&self.program, &lines);
            }
            None => {
                // SAFETY: the child has not been reaped, so the pid is ours.
                unsafe {
                    libc::kill(self.child.id() as i32, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = self.child.try_wait() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        let _ = fs::remove_file(&self.xauth_path);
    }
}

impl Drop for XServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The last few lines the X server wrote to stderr, shared with its drain
/// thread.
#[derive(Clone)]
struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES))),
        }
    }

    fn push(&self, line: String) {
        // A poisoned lock only means some previous holder panicked; the tail
        // itself is still sound, and dropping it would throw away the one
        // diagnostic this whole mechanism exists to produce.
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        while lines.len() >= TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn snapshot(&self) -> Vec<String> {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        lines.iter().cloned().collect()
    }
}

/// Forward the X server's stderr into `tail`, one line at a time.
///
/// `read_to_string` was shorter and produced nothing until EOF -- that is,
/// until the X server died, which is far too late to help anyone watching a
/// session that is merely slow to start, and no help at all when the server
/// wedges without exiting.
fn spawn_drain(stderr: ChildStderr, program: String, tail: StderrTail) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("xserver-stderr".into())
        .spawn(move || {
            let mut reader = std::io::BufReader::new(stderr);
            let mut buf = Vec::with_capacity(256);
            loop {
                buf.clear();
                // read_until rather than lines(): a single line of invalid
                // UTF-8 (a font name, say) ends an iterator of Result<String>
                // and would silence the rest of the stream.
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if let Some(line) = tidy_line(&buf) {
                    // Not warn!: Xvfb is not reliably silent, and a start that
                    // works still mentions missing font paths. Only the
                    // failure paths promote these to error!.
                    log::debug!("{program}: {line}");
                    tail.push(line);
                }
            }
        })
        .ok()
}

/// Trim one raw stderr line, or return `None` when it holds nothing to log.
///
/// Kept pure and separate because it is the part with the edge cases -- CRLF,
/// invalid UTF-8, a line long enough to swamp the tail that stores it -- and
/// the thread around it needs a real child process to exercise.
fn tidy_line(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().count() > MAX_LINE_CHARS {
        let mut short: String = text.chars().take(MAX_LINE_CHARS).collect();
        short.push('…');
        return Some(short);
    }
    Some(text.to_string())
}

/// Read the tail, first letting the drain thread catch up.
///
/// Only called once the X server is known to be dead, so its end of the pipe
/// is closed and the thread is at most one read from finishing; the deadline
/// is there for the case where something else inherited the pipe.
fn collect_tail(drain: Option<&JoinHandle<()>>, tail: &StderrTail) -> Vec<String> {
    if let Some(handle) = drain {
        let deadline = Instant::now() + DRAIN_SETTLE;
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    tail.snapshot()
}

/// Write the X server's last words to the log at `error!`.
///
/// This is the only place they are shouted about. Every line at `warn!` would
/// train an administrator to ignore the stream, and `debug!` alone left the
/// reason an X server died written down nowhere at all: the shipped unit runs
/// at `RUST_LOG=info`, which drops it.
fn report_tail(program: &str, lines: &[String]) {
    if lines.is_empty() {
        log::error!("{program} produced no output before it exited");
        return;
    }
    log::error!("last {} line(s) from {program}:", lines.len());
    for line in lines {
        log::error!("{program}: {line}");
    }
}

fn read_display_number(reader: &mut fs::File, child: &mut Child, timeout: Duration) -> Result<u32> {
    // Poll with a timeout so a hung server does not block us forever.
    let deadline = Instant::now() + timeout;
    let mut text = String::new();
    let mut buf = [0u8; 16];
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            bail!("X server exited during startup with {status}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for the X server to start");
        }
        let mut pfd = libc::pollfd {
            fd: std::os::unix::io::AsRawFd::as_raw_fd(reader),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd is valid for the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().min(200) as i32) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("poll displayfd");
        }
        if rc == 0 {
            continue;
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            bail!("X server closed displayfd without reporting a display");
        }
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
        if let Some(line) = text.lines().next() {
            if text.contains('\n') {
                return line
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| anyhow!("bad displayfd output {line:?}"));
            }
        }
    }
}

/// What to do about a directory that is more permissive than 0700.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LooseMode {
    /// Put the mode back to 0700 without asking.
    Tighten,
    /// Leave the mode alone and say what was found.
    ///
    /// For a directory an administrator configures rather than one we own
    /// outright, widening it can be deliberate -- `chgrp adm /var/log/lynxrdp`
    /// so operators can read session logs, or a traversable `/run/lynxrdp` so
    /// the optional Unix listening socket is reachable at all. Silently
    /// undoing that on every start would be a surprise, not a fix.
    Warn,
}

/// Create `dir` with mode 0700 if missing, and verify that what is there is a
/// real directory belonging to us.
///
/// `symlink_metadata` rather than `metadata` is the entire point: the question
/// is what the *name* is, not what it resolves to, because a name we do not
/// control can be pointed at somebody else's directory between two runs.
pub fn ensure_owned_dir(dir: &Path, loose: LooseMode) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("{} is a symlink; refusing to use it", dir.display());
            }
            if !meta.is_dir() {
                bail!("{} exists and is not a directory", dir.display());
            }
            if meta.uid() != crate::peer::own_uid() {
                bail!(
                    "{} is owned by uid {}, not by us",
                    dir.display(),
                    meta.uid()
                );
            }
            let mode = meta.permissions().mode() & 0o7777;
            if mode & 0o077 != 0 {
                match loose {
                    LooseMode::Tighten => {
                        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                            .with_context(|| format!("securing {}", dir.display()))?;
                    }
                    LooseMode::Warn if mode & 0o022 != 0 => log::warn!(
                        "{} is writable by other users (mode {mode:04o}); its contents \
                         can be replaced underneath us",
                        dir.display()
                    ),
                    LooseMode::Warn if mode & 0o004 != 0 => {
                        log::warn!("{} is world-readable (mode {mode:04o})", dir.display())
                    }
                    LooseMode::Warn => log::info!(
                        "{} is not private (mode {mode:04o}); leaving it as configured",
                        dir.display()
                    ),
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))
        }
        Err(e) => Err(e).with_context(|| format!("inspecting {}", dir.display())),
    }
}

/// Create `dir` with mode 0700 if missing and verify it is a private
/// directory owned by us (guards against symlink tricks in shared /tmp).
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    ensure_owned_dir(dir, LooseMode::Tighten)
}

/// Only the standard default may place its cookie outside its own directory.
/// Validate the user runtime without creating it or changing permissions on a
/// directory managed by logind. Custom paths and the /tmp fallback stay local.
fn authority_dir(runtime_dir: &Path, user_runtime: &Path, uid: u32) -> Result<PathBuf> {
    if runtime_dir != user_runtime.join("lynxrdp") {
        return Ok(runtime_dir.to_path_buf());
    }
    let meta = fs::symlink_metadata(user_runtime)
        .with_context(|| format!("inspecting {} for Xauthority", user_runtime.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        bail!(
            "{} must be a private, non-symlink directory owned by uid {uid} for Xauthority",
            user_runtime.display()
        );
    }
    Ok(user_runtime.to_path_buf())
}

/// Per-user private runtime directory for session files.
pub fn default_runtime_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(xdg);
        if p.is_dir() {
            return p.join("lynxrdp");
        }
    }
    std::env::temp_dir().join(format!("lynxrdp-{}", crate::peer::own_uid()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_authority_is_directly_in_the_private_user_runtime() {
        let user_runtime = tempfile::tempdir().unwrap();
        ensure_private_dir(user_runtime.path()).unwrap();
        let runtime = user_runtime.path().join("lynxrdp");
        // Snap's desktop policy permits xauth_* directly in the user runtime,
        // not in an application subdirectory, even when the cookie is 0600.
        assert_eq!(
            authority_dir(&runtime, user_runtime.path(), crate::peer::own_uid()).unwrap(),
            user_runtime.path()
        );
    }

    #[test]
    fn an_explicit_runtime_does_not_use_an_unrelated_user_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("custom");
        assert_eq!(
            authority_dir(
                &runtime,
                &tmp.path().join("missing"),
                crate::peer::own_uid()
            )
            .unwrap(),
            runtime
        );
    }

    #[test]
    fn authority_placement_refuses_another_users_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_private_dir(tmp.path()).unwrap();
        let runtime = tmp.path().join("lynxrdp");
        assert!(authority_dir(&runtime, tmp.path(), crate::peer::own_uid() + 1).is_err());
    }

    #[test]
    fn authority_placement_refuses_a_symlinked_user_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_private_dir(tmp.path()).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path(), &link).unwrap();
        assert!(authority_dir(&link.join("lynxrdp"), &link, crate::peer::own_uid()).is_err());
    }

    #[test]
    fn authority_placement_refuses_a_shared_runtime_without_changing_its_mode() {
        let tmp = tempfile::tempdir().unwrap();
        for mode in [0o750, 0o755, 0o770, 0o777] {
            fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode)).unwrap();
            assert!(authority_dir(
                &tmp.path().join("lynxrdp"),
                tmp.path(),
                crate::peer::own_uid()
            )
            .is_err());
            assert_eq!(fs::metadata(tmp.path()).unwrap().mode() & 0o777, mode);
        }
    }

    #[test]
    fn a_missing_xserver_executable_does_not_leave_a_cookie() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("rt");
        let cfg = XServerConfig {
            program: tmp.path().join("missing-xserver").display().to_string(),
            extra_args: vec![],
            max_width: 640,
            max_height: 480,
            dpi: 96,
            runtime_dir: runtime.clone(),
        };
        assert!(XServer::spawn(&cfg).is_err());
        assert_eq!(fs::read_dir(runtime).unwrap().count(), 0);
    }

    #[test]
    fn private_dir_is_created_and_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("rt");
        ensure_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Loosened permissions are tightened again.
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A file in the way is an error.
        let f = tmp.path().join("file");
        fs::write(&f, b"x").unwrap();
        assert!(ensure_private_dir(&f).is_err());
    }

    #[test]
    fn a_symlink_is_never_accepted_as_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // It resolves to a directory we own, so `is_dir()` would have said yes.
        assert!(link.is_dir());
        // Refused, and refused by name: `symlink_metadata` on a link already
        // reported "not a directory", which sent whoever read it looking for a
        // file that is not there instead of at the link that is.
        let e = format!("{:#}", ensure_private_dir(&link).unwrap_err());
        assert!(e.contains("symlink"), "{e}");
        assert!(ensure_owned_dir(&link, LooseMode::Warn).is_err());
    }

    #[test]
    fn a_warned_directory_keeps_the_mode_it_was_given() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("log");
        ensure_owned_dir(&d, LooseMode::Warn).unwrap();
        // An administrator widening the log directory for `adm` keeps it.
        fs::set_permissions(&d, fs::Permissions::from_mode(0o750)).unwrap();
        ensure_owned_dir(&d, LooseMode::Warn).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o750
        );
    }

    #[test]
    fn stderr_lines_are_tidied_and_bounded() {
        assert_eq!(tidy_line(b"  boom \r\n"), Some("boom".to_string()));
        assert_eq!(tidy_line(b"\n"), None);
        assert_eq!(tidy_line(b"   "), None);
        // Invalid UTF-8 must not lose the line, only the bad bytes.
        let line = tidy_line(b"bad \xff byte\n").unwrap();
        assert!(line.starts_with("bad "), "{line}");
        assert!(line.ends_with(" byte"), "{line}");
        // A very long line is truncated, and truncation is marked.
        let long = vec![b'x'; MAX_LINE_CHARS * 2];
        let line = tidy_line(&long).unwrap();
        assert_eq!(line.chars().count(), MAX_LINE_CHARS + 1);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn the_tail_keeps_only_the_last_lines() {
        let tail = StderrTail::new();
        for i in 0..TAIL_LINES * 2 {
            tail.push(format!("line {i}"));
        }
        let lines = tail.snapshot();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], format!("line {}", TAIL_LINES));
        assert_eq!(
            lines[TAIL_LINES - 1],
            format!("line {}", TAIL_LINES * 2 - 1)
        );
    }

    #[test]
    #[ignore = "needs Xvfb"]
    fn spawns_xvfb() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = XServerConfig {
            program: "Xvfb".into(),
            extra_args: vec![],
            max_width: 640,
            max_height: 480,
            dpi: 96,
            runtime_dir: tmp.path().join("rt"),
        };
        let mut xs = XServer::spawn(&cfg).unwrap();
        assert!(xs.is_running());
        assert!(xs.xauth_path().exists());
        assert_eq!(fs::metadata(xs.xauth_path()).unwrap().mode() & 0o777, 0o600);
        let mut other = XServer::spawn(&cfg).unwrap();
        assert_ne!(xs.xauth_path(), other.xauth_path());
        let d = xs.display();
        assert!(d.starts_with(':'));
        xs.shutdown();
        assert!(!xs.xauth_path().exists());
        assert!(other.xauth_path().exists());
        other.shutdown();
        assert!(!other.xauth_path().exists());
    }

    #[test]
    #[ignore = "needs Xvfb, xdpyinfo and a private /run/user/<uid> directory"]
    fn desktop_sandbox_authority_paths_authenticate_and_are_removed() {
        let user_runtime = PathBuf::from(format!("/run/user/{}", crate::peer::own_uid()));
        assert!(user_runtime.is_dir(), "needs a logind user runtime");
        for (runtime_dir, prefix) in [
            (user_runtime.join("lynxrdp"), "xauth_lynxrdp-"),
            (user_runtime.clone(), "Xauthority-"),
        ] {
            let cfg = XServerConfig {
                program: "Xvfb".into(),
                extra_args: vec![],
                max_width: 640,
                max_height: 480,
                dpi: 96,
                runtime_dir,
            };
            let mut xs = XServer::spawn(&cfg).unwrap();
            let auth = xs.xauth_path().to_path_buf();
            assert_eq!(auth.parent(), Some(user_runtime.as_path()));
            assert!(auth
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(prefix));
            assert_eq!(fs::metadata(&auth).unwrap().mode() & 0o777, 0o600);
            // Exercise the file Xvfb actually received, not just the helper's
            // path calculation. The uncredentialed connection must fail too.
            for (path, allowed) in [
                (auth.clone(), true),
                (user_runtime.join("lynxrdp-no-such-cookie"), false),
            ] {
                let status = Command::new("xdpyinfo")
                    .env("DISPLAY", xs.display())
                    .env("XAUTHORITY", path)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .unwrap();
                assert_eq!(status.success(), allowed);
            }
            xs.shutdown();
            assert!(!auth.exists());
        }
    }

    #[test]
    fn a_server_that_never_starts_fails_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let rt = tmp.path().join("rt");
        let cfg = XServerConfig {
            // Exits immediately without ever writing to displayfd, which is
            // the shape of every real "the X server would not start".
            program: "/bin/false".into(),
            extra_args: vec![],
            max_width: 640,
            max_height: 480,
            dpi: 96,
            runtime_dir: rt.clone(),
        };
        // `unwrap_err` would require XServer: Debug, and it owns a Child.
        let Err(err) = XServer::spawn(&cfg) else {
            panic!("an X server that cannot start must not spawn successfully");
        };
        let text = format!("{err:#}");
        assert!(text.contains("failed to start"), "{text}");
        // A failed start must not leave the private cookie behind.
        let leftovers: Vec<_> = fs::read_dir(&rt)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_failed_start_carries_the_servers_own_last_line() {
        let tmp = tempfile::tempdir().unwrap();
        // A stand-in X server that complains the way a real one does and
        // never writes to -displayfd. It ignores the arguments it is handed,
        // which is the only way to script this: the generated arguments come
        // first, so `/bin/sh -c` cannot be spelled through `extra_args`.
        let fake = tmp.path().join("fake-xserver");
        fs::write(
            &fake,
            "#!/bin/sh\n\
             echo 'Fatal server error:' >&2\n\
             echo '(EE) could not open default font fixed' >&2\n\
             exit 1\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let cfg = XServerConfig {
            program: fake.display().to_string(),
            extra_args: vec![],
            max_width: 640,
            max_height: 480,
            dpi: 96,
            runtime_dir: tmp.path().join("rt"),
        };
        // `unwrap_err` would require XServer: Debug, and it owns a Child.
        let Err(err) = XServer::spawn(&cfg) else {
            panic!("an X server that cannot start must not spawn successfully");
        };
        let text = format!("{err:#}");
        // The line that says what actually went wrong reaches the caller.
        assert!(text.contains("could not open default font"), "{text}");
        // And only that line: the whole blob used to be pasted into the error,
        // which is why the tail exists and why it is logged separately.
        assert!(!text.contains("Fatal server error"), "{text}");
    }
}
