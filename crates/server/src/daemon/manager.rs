//! Session lifecycle: find a user's running session or start one, and hand
//! client connections to it.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use super::users::UserInfo;
use crate::config::Config;
use crate::handoff::{send_handoff, Handoff, Reply};
use crate::session::xserver::{ensure_owned_dir, LooseMode};

/// Kernel table of open Unix sockets, used to tell whether an adopted
/// session is still there.
const PROC_NET_UNIX: &str = "/proc/net/unix";

/// How often the liveness of adopted sessions is re-checked.
///
/// `reap` runs on every pass of the accept loop -- once a second when idle --
/// and reading `/proc/net/unix` means formatting every Unix socket on the
/// host. Being half a minute out of date costs nothing: this figure only feeds
/// `count()` and the monitoring heartbeat, and an actual connection checks the
/// socket itself rather than trusting the record.
const ADOPTED_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// A running session process (its supervisor).
pub struct SessionRecord {
    /// Supervisor process, if started by this daemon instance.
    pub supervisor: Option<Child>,
    /// Control socket path.
    pub socket_path: PathBuf,
    /// Session identifier.
    pub session_id: u64,
    /// Owner's login name.
    pub username: String,
    /// When it was started.
    pub started: Instant,
}

/// Tracks sessions by uid.
pub struct SessionManager {
    cfg: Config,
    sessions: HashMap<u32, SessionRecord>,
    sessions_dir: PathBuf,
    log_dir: PathBuf,
    /// When the adopted sessions were last checked for liveness.
    last_probe: Instant,
}

impl SessionManager {
    /// Prepare runtime directories.
    pub fn new(cfg: Config) -> Result<Self> {
        let runtime_dir = cfg.session.runtime_dir.clone();
        let sessions_dir = runtime_dir.join("sessions");
        let log_dir = cfg
            .session
            .log_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("/var/log/lynxrdp"));
        // The same checks the unprivileged session applies to its own runtime
        // directory, which matter more here than they do there: this is root
        // creating files, and one of them is a socket that hands out other
        // people's connections. `is_dir()`, which is all this used to do,
        // follows symlinks and says nothing about who owns what it lands on.
        //
        // The mode is left alone for the two directories an administrator
        // configures. /run/lynxrdp may have been made traversable on purpose
        // so the optional Unix listening socket inside it can be reached.
        ensure_owned_dir(&runtime_dir, LooseMode::Warn)
            .with_context(|| format!("preparing {}", runtime_dir.display()))?;
        // The sessions directory is ours alone and is the one that must not be
        // traversable by others: whoever can reach a session socket can hand
        // it arbitrary connections.
        ensure_owned_dir(&sessions_dir, LooseMode::Tighten)
            .with_context(|| format!("preparing {}", sessions_dir.display()))?;
        // `chgrp adm /var/log/lynxrdp` is a reasonable thing for an
        // administrator to have done, so this one is reported, never changed.
        ensure_owned_dir(&log_dir, LooseMode::Warn)
            .with_context(|| format!("preparing {}", log_dir.display()))?;
        Ok(Self {
            cfg,
            sessions: HashMap::new(),
            sessions_dir,
            log_dir,
            last_probe: Instant::now(),
        })
    }

    /// Directory holding session control sockets.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Forget sessions that have ended.
    ///
    /// Two kinds of record need two mechanisms. A session this daemon started
    /// has a supervisor `Child` to wait on. One it merely adopted -- which is
    /// what happens after `systemctl try-restart`, run by every package
    /// upgrade, and deliberately survived by `KillMode=process` -- has no
    /// child, no `SIGCHLD` and nothing to wait for. Those records used to be
    /// immortal, so `count()`, and with it the monitoring heartbeat, only ever
    /// climbed; on a long-lived server the number stopped meaning anything.
    pub fn reap(&mut self) {
        let mut gone = Vec::new();
        for (uid, rec) in self.sessions.iter_mut() {
            if let Some(child) = rec.supervisor.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    log::info!(
                        "session {} for {} (uid {uid}) ended: {status}",
                        rec.session_id,
                        rec.username
                    );
                    gone.push(*uid);
                }
            }
        }
        self.probe_adopted(&mut gone);
        for uid in gone {
            if let Some(rec) = self.sessions.remove(&uid) {
                // Only unlink a socket this daemon bound itself. An adopted
                // session's socket belongs to a process we did not start and
                // have not waited for, and taking its name away while it is
                // still listening strands it: nothing can reach it again, but
                // it still holds the user's X server and logind session.
                if rec.supervisor.is_some() {
                    let _ = fs::remove_file(&rec.socket_path);
                }
            }
        }
    }

    /// Add to `gone` any adopted session whose control socket no longer has a
    /// process behind it.
    ///
    /// The evidence is `/proc/net/unix`: a bound socket vanishes from that
    /// table the moment its last holder exits, while the file it was bound to
    /// stays on disk, so the file's existence proves nothing and the table's
    /// entry proves what we want. Connecting to the socket would be a more
    /// direct test and is the wrong one -- this runs on the accept loop, where
    /// a connect to a session that is merely busy would block every other user.
    ///
    /// Fails closed. If the table cannot be read, every session stays: an
    /// over-count is a wrong number in a heartbeat, whereas an under-count
    /// makes the daemon forget a live session and start a second desktop
    /// beside it.
    fn probe_adopted(&mut self, gone: &mut Vec<u32>) {
        if self.last_probe.elapsed() < ADOPTED_PROBE_INTERVAL {
            return;
        }
        if !self.sessions.values().any(|r| r.supervisor.is_none()) {
            return;
        }
        self.last_probe = Instant::now();
        let table = match fs::read_to_string(PROC_NET_UNIX) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("cannot read {PROC_NET_UNIX} ({e}); assuming sessions are alive");
                return;
            }
        };
        for (uid, rec) in self.sessions.iter() {
            if rec.supervisor.is_some() || gone.contains(uid) {
                continue;
            }
            let path = rec.socket_path.to_string_lossy();
            if !socket_is_bound(&table, &path) {
                log::info!(
                    "adopted session for {} (uid {uid}) is gone: nothing is listening on {path}",
                    rec.username
                );
                gone.push(*uid);
            }
        }
    }

    /// Number of sessions known to be running.
    pub fn count(&self) -> usize {
        self.sessions.len()
    }

    /// Hand `client_fd` to the user's session, starting one if needed.
    pub fn handoff(&mut self, user: &UserInfo, client_fd: RawFd, peer: &str) -> Result<u64> {
        self.reap();
        let socket_path = self.sessions_dir.join(format!("{}.sock", user.uid));
        // Try an existing session first (ours, or one surviving a daemon restart).
        if socket_path.exists() {
            let known = self.sessions.contains_key(&user.uid);
            match try_handoff(&socket_path, user, client_fd, peer, Duration::from_secs(10)) {
                Ok(()) => {
                    let id = self
                        .sessions
                        .get(&user.uid)
                        .map(|r| r.session_id)
                        .unwrap_or(0);
                    log::info!(
                        "client {peer} handed to existing session for {} (uid {})",
                        user.name,
                        user.uid
                    );
                    if !known {
                        self.sessions.insert(
                            user.uid,
                            SessionRecord {
                                supervisor: None,
                                socket_path: socket_path.clone(),
                                session_id: 0,
                                username: user.name.clone(),
                                started: Instant::now(),
                            },
                        );
                    }
                    return Ok(id);
                }
                Err(e) => {
                    log::warn!(
                        "existing session for {} unusable ({e:#}); starting a new one",
                        user.name
                    );
                    if let Some(mut rec) = self.sessions.remove(&user.uid) {
                        if let Some(c) = rec.supervisor.as_mut() {
                            terminate(c, HANDOFF_TERMINATE_GRACE);
                        }
                    }
                    let _ = fs::remove_file(&socket_path);
                }
            }
        }
        let session_id = new_session_id();
        let supervisor = self
            .spawn(user, &socket_path, session_id)
            .with_context(|| format!("starting session for {}", user.name))?;
        self.sessions.insert(
            user.uid,
            SessionRecord {
                supervisor: Some(supervisor),
                socket_path: socket_path.clone(),
                session_id,
                username: user.name.clone(),
                started: Instant::now(),
            },
        );
        // The session process accepts once its X server is up; the backlog
        // holds our connection until then.
        match try_handoff(&socket_path, user, client_fd, peer, Duration::from_secs(45)) {
            Ok(()) => {
                log::info!(
                    "client {peer} handed to new session {session_id} for {} (uid {})",
                    user.name,
                    user.uid
                );
                Ok(session_id)
            }
            Err(e) => {
                if let Some(mut rec) = self.sessions.remove(&user.uid) {
                    if let Some(c) = rec.supervisor.as_mut() {
                        terminate(c, HANDOFF_TERMINATE_GRACE);
                    }
                }
                let _ = fs::remove_file(&socket_path);
                Err(e).context("new session did not accept the connection")
            }
        }
    }

    fn spawn(&self, user: &UserInfo, socket_path: &Path, session_id: u64) -> Result<Child> {
        let _ = fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;
        let log_path = self.log_dir.join(format!("{}.log", user.name));
        let log_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_path)
            .with_context(|| format!("opening {}", log_path.display()))?;
        if crate::peer::own_uid() == 0 {
            // SAFETY: chown on a path we just created.
            let c = std::ffi::CString::new(log_path.as_os_str().as_encoded_bytes())?;
            unsafe {
                libc::chown(c.as_ptr(), user.uid, user.gid);
            }
        }
        let s = &self.cfg.session;
        let mut session_args: Vec<String> = vec![
            "--width".into(),
            s.default_width.to_string(),
            "--height".into(),
            s.default_height.to_string(),
            "--max-width".into(),
            s.max_width.to_string(),
            "--max-height".into(),
            s.max_height.to_string(),
            "--dpi".into(),
            s.dpi.to_string(),
            "--xserver".into(),
            s.xserver.clone(),
            "--startwm".into(),
            s.startwm.clone(),
            "--max-fps".into(),
            s.max_fps.to_string(),
            "--max-in-flight".into(),
            s.max_in_flight.to_string(),
            "--idle-timeout".into(),
            s.idle_timeout_secs.to_string(),
            "--session-id".into(),
            session_id.to_string(),
        ];
        for a in &s.xserver_args {
            session_args.push("--xserver-arg".into());
            session_args.push(a.clone());
        }
        let exe = std::env::current_exe().context("locating lynxrdpd executable")?;
        let mut cmd = Command::new(exe);
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
            .arg(&user.shell)
            .arg("--control-fd")
            .arg("3")
            .arg("--log-fd")
            .arg("4")
            .arg("--session-binary")
            .arg(&s.session_binary);
        if !s.pam_service.is_empty() {
            cmd.arg("--pam-service").arg(&s.pam_service);
        }
        cmd.arg("--").args(&session_args);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let lfd = listener.as_raw_fd();
        let logfd = log_file.as_raw_fd();
        // SAFETY: only dup2/close/setsid before exec.
        unsafe {
            cmd.pre_exec(move || {
                let l = libc::fcntl(lfd, libc::F_DUPFD, 10);
                let g = libc::fcntl(logfd, libc::F_DUPFD, 10);
                if l < 0 || g < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::dup2(l, 3) < 0 || libc::dup2(g, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(l);
                libc::close(g);
                libc::setsid();
                Ok(())
            });
        }
        let child = cmd.spawn().context("spawning session supervisor")?;
        log::info!(
            "started supervisor pid {} for {} (uid {}), session {session_id}, log {}",
            child.id(),
            user.name,
            user.uid,
            log_path.display()
        );
        // Close our copies: the supervisor/session own them now. Keeping the
        // listener open here would make dead sessions look alive.
        drop(listener);
        drop(log_file);
        Ok(child)
    }

    /// Terminate every session started by this daemon (used by tests and
    /// `--stop-sessions`; a normal daemon exit leaves sessions running).
    ///
    /// Sessions this daemon only adopted are left strictly alone, socket
    /// included. Unlinking one was quietly destructive: nothing signalled the
    /// session, so it keeps running with the user's X server and logind
    /// session inside it, but its name is gone, so the next daemon cannot find
    /// it and starts a second desktop for a user who already has one.
    pub fn stop_all(&mut self) {
        for (uid, mut rec) in self.sessions.drain() {
            match rec.supervisor.as_mut() {
                Some(child) => terminate(child, Duration::from_secs(5)),
                None => {
                    log::info!(
                        "leaving adopted session for {} (uid {uid}) running",
                        rec.username
                    );
                    continue;
                }
            }
            let _ = fs::remove_file(&rec.socket_path);
        }
    }
}

/// How long a supervisor gets to shut down cleanly when a handoff has failed.
///
/// Deliberately shorter than `stop_all`'s: this runs on the daemon's single
/// accept loop, where every extra second is a second no other user can connect.
const HANDOFF_TERMINATE_GRACE: Duration = Duration::from_secs(1);

/// Stop a supervisor politely, and only then insistently.
///
/// SIGKILL on its own is wrong here. The supervisor holds the PAM session open
/// and only its SIGTERM handler runs `pam_close_session`, so killing it
/// outright leaked a logind session on every failed attempt -- and, because
/// `lynxrdp-session` had no parent-death link, the whole desktop with it.
fn terminate(child: &mut Child, grace: Duration) {
    // SAFETY: signalling our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether anything in a `/proc/net/unix` dump is bound to `path`.
///
/// The path is whatever follows the seven numeric columns, and only a socket
/// with a name of its own has one -- a client that merely connected to a
/// session is unbound and prints no path at all -- so a match means the
/// listener itself is still open somewhere.
///
/// The one false positive is a socket whose file has already been unlinked:
/// the kernel keeps printing the name it was bound to. That costs another
/// interval of over-counting and never a live session forgotten, which is the
/// direction this check is meant to err in.
fn socket_is_bound(proc_net_unix: &str, path: &str) -> bool {
    proc_net_unix
        .lines()
        .skip(1)
        .any(|row| bound_path(row) == Some(path))
}

/// The name one `/proc/net/unix` row is bound to, or `None` for a socket that
/// has none.
///
/// The seven leading columns are fixed and numeric, so they are skipped by
/// counting; everything after them is the name, taken whole. Splitting the
/// whole row on whitespace and picking the eighth field would have been
/// shorter and wrong in one direction that matters: a bound path may contain
/// a space, `runtime_dir` is something an administrator configures, and the
/// failure would be silent -- every adopted session read as dead because its
/// path never matched.
fn bound_path(row: &str) -> Option<&str> {
    let mut rest = row.trim_start();
    for _ in 0..7 {
        let end = rest.find(char::is_whitespace)?;
        rest = rest[end..].trim_start();
    }
    let rest = rest.trim_end();
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn try_handoff(
    socket_path: &Path,
    user: &UserInfo,
    client_fd: RawFd,
    peer: &str,
    timeout: Duration,
) -> Result<()> {
    let control = UnixStream::connect(socket_path)
        .with_context(|| format!("connecting to {}", socket_path.display()))?;
    // Check who put this socket here before posting somebody's connection
    // through it. SO_PEERCRED on the connecting end reports the credentials of
    // the process that called listen(2), and that is this daemon rather than
    // the session -- the listener is created in `spawn` and inherited as fd 3
    // -- so what this really asks is "is this still our socket". Nothing else
    // should be able to answer, `sessions_dir` being 0700 and ours; that is the
    // point of checking rather than a reason not to. It is the mirror of the
    // session's own peer check on the accepting end, and together the two make
    // the handoff safe to reason about without leaning on directory
    // permissions as the only thing standing in the way.
    let owner = crate::peer::unix_peer(&control)
        .with_context(|| format!("identifying the owner of {}", socket_path.display()))?;
    let own = crate::peer::own_uid();
    if owner.uid != own && owner.uid != 0 {
        bail!(
            "{} is served by uid {}, not by uid {own} or root",
            socket_path.display(),
            owner.uid
        );
    }
    control.set_read_timeout(Some(timeout))?;
    let h = Handoff {
        uid: user.uid,
        username: user.name.clone(),
        peer: peer.to_string(),
    };
    match send_handoff(&control, &h, client_fd, timeout)? {
        Reply::Accepted => Ok(()),
        Reply::Refused => bail!("session refused the handoff"),
    }
}

fn new_session_id() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    (t ^ (pid << 48)) & 0x7fff_ffff_ffff_ffff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `/proc/net/unix` extract: one bound listener, one connected
    /// socket with no name of its own, one abstract name, and one path with a
    /// space in it (which the kernel prints verbatim).
    const PROC_NET_UNIX_SAMPLE: &str = "\
Num       RefCount Protocol Flags    Type St Inode Path
0000000000000000: 00000002 00000000 00010000 0001 01 21456 /run/lynxrdp/sessions/1000.sock
0000000000000000: 00000003 00000000 00000000 0001 03 21457
0000000000000000: 00000002 00000000 00010000 0001 01 21460 @/tmp/.X11-unix/X0
0000000000000000: 00000002 00000000 00010000 0001 01 21470 /srv/my sessions/1002.sock
";

    #[test]
    fn a_bound_socket_is_recognised() {
        assert!(socket_is_bound(
            PROC_NET_UNIX_SAMPLE,
            "/run/lynxrdp/sessions/1000.sock"
        ));
        assert!(socket_is_bound(PROC_NET_UNIX_SAMPLE, "@/tmp/.X11-unix/X0"));
        // `runtime_dir` is configuration, so the path is not guaranteed to be
        // one whitespace-free field. Reading only as far as the first space
        // would report this live session as gone every 30 seconds.
        assert!(socket_is_bound(
            PROC_NET_UNIX_SAMPLE,
            "/srv/my sessions/1002.sock"
        ));
        assert!(!socket_is_bound(PROC_NET_UNIX_SAMPLE, "/srv/my"));
    }

    #[test]
    fn an_absent_socket_is_not_mistaken_for_a_live_one() {
        // The session that ended: its file may still be on disk, but nothing
        // in the table is bound to it.
        assert!(!socket_is_bound(
            PROC_NET_UNIX_SAMPLE,
            "/run/lynxrdp/sessions/1001.sock"
        ));
        // A prefix of a real path is not a match.
        assert!(!socket_is_bound(
            PROC_NET_UNIX_SAMPLE,
            "/run/lynxrdp/sessions/1000.soc"
        ));
        // A connected socket prints no path; an empty path matches nothing.
        assert!(!socket_is_bound(PROC_NET_UNIX_SAMPLE, ""));
        // Neither the header line nor an empty table is ever a match.
        assert!(!socket_is_bound(PROC_NET_UNIX_SAMPLE, "Path"));
        assert!(!socket_is_bound("", "/run/lynxrdp/sessions/1000.sock"));
    }
}
