//! Session lifecycle: find a user's running session or start one, and hand
//! client connections to it.
//!
//! # Why this is concurrent
//!
//! Handing a connection over is the slowest thing the daemon does, and how
//! slow is not bounded by anything the daemon controls. A cold start budgets
//! forty-five seconds -- the session waits twenty for Xvfb's displayfd and ten
//! more to connect to it, and that budget is honest rather than generous -- and
//! a failed attempt then adds a SIGTERM, a grace period and a respawn. A
//! session that has been stopped (`kill -STOP`, which any user may do to their
//! own processes) never answers at all.
//!
//! All of that used to run on the daemon's single accept loop, so one user
//! could stall every other user's connection for as long as they cared to, and
//! ten people logging in at nine o'clock serialised behind each other's cold
//! starts with no attacker involved at all.
//!
//! So the loop now accepts, identifies the peer and applies the access policy
//! -- microseconds, and the `/proc/net/tcp` lookup *must* happen there while
//! the peer's socket is still in the kernel's table -- and hands the rest to
//! [`HandoffPool`]. Three things keep that from turning one problem into
//! several:
//!
//! * The client socket becomes an [`OwnedFd`] the moment it leaves the
//!   listener, and the pool owns it until the worker is finished. There is
//!   exactly one owner at every instant and exactly one close. Passing a
//!   `RawFd` across a thread boundary instead is how this becomes a
//!   use-after-close.
//! * `Starts` serialises by uid. Two connections from one user arriving
//!   together would otherwise both find no socket, both spawn, and the second
//!   spawn's `remove_file` + `bind` would take the name away from the first
//!   supervisor -- which then holds a live X server that nothing can reach
//!   again. `reap` claims the same latch before it unlinks, for the same
//!   reason and against the same failure.
//! * `Admission` bounds how much of the pool one uid may hold, because the
//!   start latch alone would let a user with a stopped session park a worker
//!   per connection they open.
//!
//! The session map is behind a mutex that is held only for map operations.
//! Nothing slow -- no connect, no reply wait, no `terminate`, no filesystem
//! call -- happens with it held.
//!
//! Where both locks are taken the order is always the start latch first and
//! the map second, in `handoff` and in `reap` alike. A path that took the map
//! and then went for the latch would deadlock against either of them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use lynxrdp_proto::message::reject;

use super::send_rejection;
use super::supervisor::SESSION_STOP_GRACE;
use super::users::UserInfo;
use crate::config::{Config, SessionConfig};
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

/// Threads that perform handoffs.
///
/// Each is parked and costs nothing until a connection arrives, so this is
/// sized for the worst shape rather than the common one: a user with a
/// connection in flight and a second waiting on the start latch holds two
/// workers, so sixteen keeps at least eight *distinct* users making progress
/// even when every one of them is double-connecting.
///
/// It is deliberately not an attempt to limit concurrent cold starts. What
/// bounds those is the host -- sixteen simultaneous X servers and desktops is
/// real load -- and throttling them here would only move the queue, since
/// those users are going to start those desktops either way.
const HANDOFF_WORKERS: usize = 16;

/// Connections that may wait for a worker before the daemon starts refusing.
///
/// Refusing is the point. A queue that grew instead would hold client sockets
/// and their memory for as long as one stopped session cared to stall, and
/// would eventually hand a worker a connection whose client gave up minutes
/// ago. A rejection the client can show a person beats a wait it cannot.
const HANDOFF_QUEUE: usize = 32;

/// Connections one uid may have queued or in flight at once.
///
/// Two, because [`Starts`] serialises a uid's connections anyway and a second
/// connection from one user *replaces* the first at the session: a third
/// waiting behind them would have nothing left to do by the time it arrived.
/// Without this cap the start latch becomes the attack -- one user opens
/// sixteen connections, stops their own session, and every worker is parked
/// waiting for a uid that will never make progress.
const PER_UID_IN_FLIGHT: usize = 2;

/// How long a session that is already running gets to answer a handoff.
///
/// It has an accept loop and nothing to build, so this covers a loaded host
/// rather than any real work.
const ESTABLISHED_REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a session that has just been spawned gets to answer.
///
/// The session allows twenty seconds for Xvfb's displayfd and ten more to
/// connect to it, so anything under about thirty-five declares healthy
/// sessions dead on a busy machine. This is the timeout whose cost -- one cold
/// start stalling every other connection -- the worker pool exists to remove.
const COLD_START_REPLY_TIMEOUT: Duration = Duration::from_secs(45);

/// How long a supervisor gets to exit after SIGTERM before it is killed.
///
/// A supervisor answers SIGTERM by forwarding it, waiting up to
/// [`SESSION_STOP_GRACE`] for the session to leave, killing it if it has not,
/// and only then closing the PAM session. A grace shorter than that does not
/// hurry anything; it skips the close. This was one second on a failed
/// handoff, on the reasoning that a supervisor which had not exited within a
/// second was not going to -- but it had answered, and was waiting for a
/// desktop that takes longer than that to exit, so every such desktop leaked
/// its logind session. The user at the end of it still waits, but only after
/// a cold start that has already failed, and never past this.
const SUPERVISOR_EXIT_GRACE: Duration = Duration::from_secs(SESSION_STOP_GRACE.as_secs() + 3);

/// Take a lock, ignoring poisoning.
///
/// A worker that panics part-way through a handoff must not take the whole
/// daemon with it: every other user would be locked out of a process that is
/// otherwise healthy, and the sessions already running would keep running with
/// nothing able to reach them. What these mutexes guard is a map of records, a
/// set of uids and a map of counters, and no operation on any of them leaves a
/// half-updated value behind, so there is no invariant here for poisoning to
/// protect.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

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
    ///
    /// Also serves as the record's identity. `reap` reads the map, lets go of
    /// it to touch `/proc`, and takes it again to remove what it found dead;
    /// a worker may have replaced a record for that uid in between, and
    /// removing *that* one would unlink a live session's socket.
    pub started: Instant,
}

/// The mutable half of the manager.
struct Inner {
    sessions: HashMap<u32, SessionRecord>,
    /// When the adopted sessions were last checked for liveness.
    last_probe: Instant,
}

/// One adopted session, copied out of the map so `/proc/net/unix` can be read
/// with the lock released.
struct Adopted {
    uid: u32,
    username: String,
    socket_path: PathBuf,
    started: Instant,
}

/// Tracks sessions by uid.
///
/// Every method takes `&self`: the accept loop reaps and counts while the
/// pool's workers hand connections over, all through one `Arc`.
pub struct SessionManager {
    cfg: Config,
    inner: Mutex<Inner>,
    starts: Starts,
    sessions_dir: PathBuf,
    log_dir: PathBuf,
    /// Set by [`HandoffPool::shutdown`]; no session is started after it.
    stopping: AtomicBool,
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
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                last_probe: Instant::now(),
            }),
            starts: Starts::default(),
            sessions_dir,
            log_dir,
            stopping: AtomicBool::new(false),
        })
    }

    /// Directory holding session control sockets.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Start nothing new from here on: a handoff that reaches the point of
    /// spawning after this refuses instead.
    pub fn begin_shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Whether [`begin_shutdown`](Self::begin_shutdown) has been called.
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
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
    ///
    /// The map lock is taken in short bursts and held for neither the `/proc`
    /// read nor the unlinks, so a worker's handoff is never waiting on this.
    pub fn reap(&self) {
        let mut ended: Vec<(u32, SessionRecord)> = Vec::new();
        let mut adopted: Vec<Adopted> = Vec::new();
        {
            let mut inner = lock(&self.inner);
            let mut exited: Vec<u32> = Vec::new();
            for (uid, rec) in inner.sessions.iter_mut() {
                if let Some(child) = rec.supervisor.as_mut() {
                    if let Ok(Some(status)) = child.try_wait() {
                        log::info!(
                            "session {} for {} (uid {uid}) ended: {status}",
                            rec.session_id,
                            rec.username
                        );
                        exited.push(*uid);
                    }
                }
            }
            for uid in exited {
                if let Some(rec) = inner.sessions.remove(&uid) {
                    ended.push((uid, rec));
                }
            }
            if inner.last_probe.elapsed() >= ADOPTED_PROBE_INTERVAL {
                for (uid, rec) in inner.sessions.iter() {
                    if rec.supervisor.is_none() {
                        adopted.push(Adopted {
                            uid: *uid,
                            username: rec.username.clone(),
                            socket_path: rec.socket_path.clone(),
                            started: rec.started,
                        });
                    }
                }
                // Only count it as a probe if there was something to probe, so
                // a daemon with no adopted sessions does not spend its next
                // half minute refusing to look.
                if !adopted.is_empty() {
                    inner.last_probe = Instant::now();
                }
            }
        }
        // Every record here owned a supervisor that has exited, so the socket
        // is ours to unlink -- see `probe_adopted` for the case where it is
        // emphatically not.
        //
        // Ours, but only while nothing is putting a new socket at that name.
        // `spawn` unlinks and binds this exact path, and it does so with the
        // map unlocked, so the two have to be made exclusive by something
        // other than the map: the uid's start latch, which `handoff` holds
        // across bind-and-insert. Holding it here means no handoff for this
        // uid is in that window, and an empty map entry taken under it then
        // means there is no newer session either -- one may have finished
        // between the removal above and the claim below. Without both checks
        // this unlinks a socket a worker bound seconds ago, and the desktop
        // behind it becomes unreachable while still holding the user's X
        // server: the exact failure the latch was introduced to prevent, let
        // back in through the reaper.
        //
        // `try_acquire` and never `acquire`, because this runs on the accept
        // loop, which must not wait behind a cold start. The record is still
        // forgotten either way -- only the unlink is skipped -- and a later
        // pass will not retry it, because there is no record left to reap. It
        // does not need to: the handoff that was holding the latch is the one
        // thing that path leads to, and it unlinks the stale name itself
        // before `spawn` binds a new one.
        for (uid, rec) in ended {
            let Some(_claim) = self.starts.try_acquire(uid) else {
                continue;
            };
            if lock(&self.inner).sessions.contains_key(&uid) {
                continue;
            }
            let _ = fs::remove_file(&rec.socket_path);
        }
        if !adopted.is_empty() {
            self.probe_adopted(&adopted);
        }
    }

    /// Drop any adopted session whose control socket no longer has a process
    /// behind it.
    ///
    /// The evidence is `/proc/net/unix`: a bound socket vanishes from that
    /// table the moment its last holder exits, while the file it was bound to
    /// stays on disk, so the file's existence proves nothing and the table's
    /// entry proves what we want. Connecting to the socket would be a more
    /// direct test and is the wrong one -- a connect to a session that is
    /// merely busy would take one of a small number of workers with it.
    ///
    /// Fails closed. If the table cannot be read, every session stays: an
    /// over-count is a wrong number in a heartbeat, whereas an under-count
    /// makes the daemon forget a live session and start a second desktop
    /// beside it.
    ///
    /// Nothing is unlinked. An adopted session's socket belongs to a process
    /// we did not start and have not waited for, and this decides only that it
    /// is *probably* gone; taking the name away from one that is still
    /// listening strands it, holding the user's X server and logind session
    /// where nothing can reach them.
    fn probe_adopted(&self, candidates: &[Adopted]) {
        let table = match fs::read_to_string(PROC_NET_UNIX) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("cannot read {PROC_NET_UNIX} ({e}); assuming sessions are alive");
                return;
            }
        };
        let mut gone: Vec<(u32, Instant)> = Vec::new();
        for a in candidates {
            let path = a.socket_path.to_string_lossy();
            if !socket_is_bound(&table, &path) {
                log::info!(
                    "adopted session for {} (uid {}) is gone: nothing is listening on {path}",
                    a.username,
                    a.uid
                );
                gone.push((a.uid, a.started));
            }
        }
        if gone.is_empty() {
            return;
        }
        let mut inner = lock(&self.inner);
        for (uid, started) in gone {
            // The map was unlocked while `/proc` was read, and a worker may
            // have replaced this uid's record with a session it has just
            // started. Matching on `started` is what keeps this from throwing
            // away a session that is seconds old and very much alive.
            let same = inner
                .sessions
                .get(&uid)
                .is_some_and(|rec| rec.started == started);
            if same {
                inner.sessions.remove(&uid);
            }
        }
    }

    /// Number of sessions known to be running.
    pub fn count(&self) -> usize {
        lock(&self.inner).sessions.len()
    }

    /// Hand `client_fd` to the user's session, starting one if needed.
    ///
    /// The descriptor is *borrowed*: the session receives a duplicate of its
    /// own through `SCM_RIGHTS`, and the caller's copy stays the caller's to
    /// close. This ran on the accept loop and took a `RawFd`; both changed
    /// together, because a raw descriptor crossing a thread boundary is how
    /// two owners and one double close get introduced.
    pub fn handoff(&self, user: &UserInfo, client_fd: BorrowedFd<'_>, peer: &str) -> Result<u64> {
        self.handoff_with(user, client_fd, peer, ESTABLISHED_REPLY_TIMEOUT)
    }

    /// [`handoff`](Self::handoff) with the established-session reply timeout
    /// as a parameter, so a test can exercise a session that never answers
    /// without waiting out the real ten seconds.
    fn handoff_with(
        &self,
        user: &UserInfo,
        client_fd: BorrowedFd<'_>,
        peer: &str,
        established_timeout: Duration,
    ) -> Result<u64> {
        // Everything below is serialised for this uid and for no other. Two
        // connections from one user arriving together would otherwise both
        // find no socket, both spawn, and the second spawn's `remove_file` +
        // `bind` would orphan the first supervisor with the user's desktop
        // still inside it.
        let _starting = self.starts.acquire(user.uid);
        // Checked again here, and not only by the worker before it took the
        // job: the wait just above can be a whole cold start for the same uid,
        // and a daemon told to stop in the meantime has no business spawning
        // a desktop it will never record. Two cold starts back to back were
        // past systemd's stop timeout, and the SIGKILL that followed left a
        // half-spawned supervisor with no record and no daemon.
        if self.is_stopping() {
            bail!("the daemon is shutting down");
        }
        self.reap();
        let socket_path = self.sessions_dir.join(format!("{}.sock", user.uid));
        // Try an existing session first (ours, or one surviving a daemon restart).
        if socket_path.exists() {
            match try_handoff(&socket_path, user, client_fd, peer, established_timeout) {
                Ok(()) => {
                    // Looked up and adopted under one lock. Reading whether
                    // the session was known beforehand and acting on it
                    // afterwards leaves a window in which `reap` drops the
                    // record, and the daemon then forgets a session it is
                    // holding a live connection to.
                    let id = {
                        let mut inner = lock(&self.inner);
                        match inner.sessions.get(&user.uid) {
                            Some(rec) => rec.session_id,
                            None => {
                                // A session that survived a daemon restart.
                                // Its id is not ours to know -- we never
                                // started it -- so the record carries 0.
                                inner.sessions.insert(
                                    user.uid,
                                    SessionRecord {
                                        supervisor: None,
                                        socket_path: socket_path.clone(),
                                        session_id: 0,
                                        username: user.name.clone(),
                                        started: Instant::now(),
                                    },
                                );
                                0
                            }
                        }
                    };
                    log::info!(
                        "client {peer} handed to existing session for {} (uid {})",
                        user.name,
                        user.uid
                    );
                    return Ok(id);
                }
                // Something is listening and did not take the connection, and
                // it is not a process this daemon started: a session adopted
                // after a restart, or one not yet even recorded. There is no
                // supervisor to signal, so taking its name and starting a
                // second desktop beside it would strand it -- Xvfb, the
                // desktop and the logind session all still running where
                // nothing can reach them, which `probe_adopted` and `stop_all`
                // go to some length to avoid and this path used to do
                // anyway, on nothing more than a ten-second reply timeout that
                // a swapping host, or the user's own `kill -STOP`, can cause.
                // The connection is refused instead and the session left
                // exactly as it was; a session that has really gone answers
                // the next attempt with ECONNREFUSED and is replaced then.
                Err(HandoffFailure::Unusable(e)) if !self.owns_session(user.uid) => {
                    log::warn!(
                        "existing session for {} did not take the connection ({e:#}); \
                         it was not started by this daemon and is left running",
                        user.name
                    );
                    return Err(e).with_context(|| {
                        format!(
                            "the existing session for {} is still running but did not take \
                             the connection; try again once it is responsive",
                            user.name
                        )
                    });
                }
                Err(failure) => {
                    let e = failure.into_error();
                    log::warn!(
                        "existing session for {} unusable ({e:#}); starting a new one",
                        user.name
                    );
                    // Take the record out under the lock and stop the process
                    // outside it: `terminate` waits out a grace period, and no
                    // other user's handoff may be held up by one supervisor
                    // taking its time to close a PAM session.
                    let stale = {
                        let mut inner = lock(&self.inner);
                        inner.sessions.remove(&user.uid)
                    };
                    if let Some(mut rec) = stale {
                        if let Some(c) = rec.supervisor.as_mut() {
                            terminate(c, SUPERVISOR_EXIT_GRACE);
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
        {
            let mut inner = lock(&self.inner);
            inner.sessions.insert(
                user.uid,
                SessionRecord {
                    supervisor: Some(supervisor),
                    socket_path: socket_path.clone(),
                    session_id,
                    username: user.name.clone(),
                    started: Instant::now(),
                },
            );
        }
        // The session process accepts once its X server is up; the backlog
        // holds our connection until then.
        match try_handoff(
            &socket_path,
            user,
            client_fd,
            peer,
            COLD_START_REPLY_TIMEOUT,
        ) {
            Ok(()) => {
                log::info!(
                    "client {peer} handed to new session {session_id} for {} (uid {})",
                    user.name,
                    user.uid
                );
                Ok(session_id)
            }
            Err(failure) => {
                // Safe to remove by uid alone: the start latch means no other
                // thread can have inserted a record for this user, and if
                // `reap` beat us to it there is nothing left to stop.
                let failed = {
                    let mut inner = lock(&self.inner);
                    inner.sessions.remove(&user.uid)
                };
                if let Some(mut rec) = failed {
                    if let Some(c) = rec.supervisor.as_mut() {
                        terminate(c, SUPERVISOR_EXIT_GRACE);
                    }
                }
                let _ = fs::remove_file(&socket_path);
                Err(failure.into_error()).context("new session did not accept the connection")
            }
        }
    }

    /// Whether the session recorded for `uid` is one this daemon started, and
    /// so one it holds a supervisor for and can end.
    fn owns_session(&self, uid: u32) -> bool {
        lock(&self.inner)
            .sessions
            .get(&uid)
            .is_some_and(|rec| rec.supervisor.is_some())
    }

    fn spawn(&self, user: &UserInfo, socket_path: &Path, session_id: u64) -> Result<Child> {
        let _ = fs::remove_file(socket_path);
        let listener = UnixListener::bind(socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;
        let (log_path, log_file) = open_session_log(&self.log_dir, user)?;
        let s = &self.cfg.session;
        let session_args = session_argv(s, session_id);
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
    ///
    /// Run [`HandoffPool::shutdown`] first. A worker part-way through a spawn
    /// would otherwise insert its record after the map had been drained, and
    /// that session would outlive the daemon it was asked to end with it.
    ///
    /// Every supervisor is signalled before any is waited for, so the whole
    /// thing takes one grace period rather than one per session: each
    /// supervisor may now take up to [`SUPERVISOR_EXIT_GRACE`] to see its
    /// desktop out, and a dozen of them in turn would run past systemd's stop
    /// timeout.
    pub fn stop_all(&self) {
        let records: Vec<(u32, SessionRecord)> = {
            let mut inner = lock(&self.inner);
            inner.sessions.drain().collect()
        };
        let mut ours: Vec<SessionRecord> = Vec::new();
        for (uid, rec) in records {
            if rec.supervisor.is_some() {
                ours.push(rec);
            } else {
                log::info!(
                    "leaving adopted session for {} (uid {uid}) running",
                    rec.username
                );
            }
        }
        let children: Vec<&mut Child> = ours
            .iter_mut()
            .filter_map(|rec| rec.supervisor.as_mut())
            .collect();
        terminate_all(children, SUPERVISOR_EXIT_GRACE);
        for rec in &ours {
            let _ = fs::remove_file(&rec.socket_path);
        }
    }
}

/// Open the user's session log for appending, creating it if need be.
///
/// Without `O_NOFOLLOW` this was root appending to, and then `chown`ing,
/// whatever `<user>.log` pointed at. The packaged directory is 0700 root and
/// safe, but `log_dir` is configuration and `ensure_owned_dir` only warns when
/// it has been widened -- `chgrp adm` so operators can rotate by hand is a
/// reasonable thing to have done -- and in that configuration anyone able to
/// create entries there could plant `alice.log -> /etc/sudoers`, connect as
/// alice, and have root append to it and hand it to her.
/// `fs.protected_symlinks` covers only sticky world-writable directories, so a
/// 0775 root:adm one is unprotected. The ownership change goes through the
/// descriptor for the same reason: a path can be swapped between two calls
/// and a descriptor cannot.
fn open_session_log(log_dir: &Path, user: &UserInfo) -> Result<(PathBuf, fs::File)> {
    let log_path = log_dir.join(format!("{}.log", user.name));
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    if crate::peer::own_uid() == 0 {
        // SAFETY: fchown on a descriptor this process holds open.
        let rc = unsafe { libc::fchown(log_file.as_raw_fd(), user.uid, user.gid) };
        if rc != 0 {
            // The session can still write to it -- the descriptor is already
            // open -- so this costs the user only the ability to read their
            // own log, which is worth a line and not a refused session.
            log::warn!(
                "could not give {} to {}: {}",
                log_path.display(),
                user.name,
                std::io::Error::last_os_error()
            );
        }
    }
    Ok((log_path, log_file))
}

/// One-at-a-time admission per uid.
///
/// A plain `Mutex` per uid would need a map of mutexes and a rule for when an
/// entry may be taken out of it again; a set of the uids currently starting,
/// with a condvar to wake whoever is waiting on one, needs neither. The set is
/// bounded by the number of handoffs in flight, and an entry is gone the
/// moment its guard drops.
#[derive(Default)]
struct Starts {
    busy: Mutex<HashSet<u32>>,
    freed: Condvar,
}

/// Held for as long as one uid's handoff is in progress.
struct Starting<'a> {
    starts: &'a Starts,
    uid: u32,
}

impl Starts {
    /// Wait until no other handoff is running for `uid`, then claim it.
    fn acquire(&self, uid: u32) -> Starting<'_> {
        let mut busy = lock(&self.busy);
        while !busy.insert(uid) {
            busy = self.freed.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        Starting { starts: self, uid }
    }

    /// Claim `uid` if it is free, and never wait for it.
    ///
    /// For [`SessionManager::reap`], which needs the same exclusion with
    /// `spawn` that `handoff` has but runs on the accept loop, where waiting
    /// for a cold start is precisely what must not happen. Refusing costs it
    /// one deferred `unlink` and nothing else.
    fn try_acquire(&self, uid: u32) -> Option<Starting<'_>> {
        let mut busy = lock(&self.busy);
        if busy.insert(uid) {
            Some(Starting { starts: self, uid })
        } else {
            None
        }
    }
}

impl Drop for Starting<'_> {
    fn drop(&mut self) {
        lock(&self.starts.busy).remove(&self.uid);
        // Everyone is woken because a condvar cannot tell waiters on different
        // uids apart. That is a herd of at most `HANDOFF_WORKERS` threads,
        // each of which re-checks a hash set and goes back to sleep, so a
        // condvar per uid would buy nothing but a map to keep it in.
        self.starts.freed.notify_all();
    }
}

/// How many connections each uid has queued or in flight.
///
/// See [`PER_UID_IN_FLIGHT`] for why this exists at all: without it one user
/// can hold every worker in the pool simply by opening connections to a
/// session they have stopped.
#[derive(Default)]
struct Admission {
    in_flight: Mutex<HashMap<u32, usize>>,
}

/// One admitted connection's place in the pool.
///
/// Dropping it gives the place back, which is why it travels inside the job
/// rather than being released by the worker: a job discarded at shutdown,
/// still sitting in the queue, must release its place too.
struct Slot {
    admission: Arc<Admission>,
    uid: u32,
}

impl Admission {
    /// Claim a place for `uid`, or `None` when it already holds `cap` of them.
    ///
    /// Not a method on `&self` because the slot has to keep the `Admission`
    /// alive, and `self: &Arc<Self>` is not a receiver Rust will take.
    fn take(admission: &Arc<Self>, uid: u32, cap: usize) -> Option<Slot> {
        debug_assert!(cap >= 1, "a cap of zero would admit nobody");
        let mut in_flight = lock(&admission.in_flight);
        // At the cap the entry already exists and is non-zero, so `or_insert`
        // never leaves a zero behind on the refusal path.
        let n = in_flight.entry(uid).or_insert(0);
        if *n >= cap {
            return None;
        }
        *n += 1;
        Some(Slot {
            admission: admission.clone(),
            uid,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut in_flight = lock(&self.admission.in_flight);
        if let Some(n) = in_flight.get_mut(&self.uid) {
            *n -= 1;
            // Removed at zero so the map is bounded by who is connecting now
            // rather than by everyone who ever has.
            if *n == 0 {
                in_flight.remove(&self.uid);
            }
        }
    }
}

/// A connection waiting for a worker.
struct Job {
    /// The client's socket.
    ///
    /// The pool owns it from `submit` until the worker is done, and the
    /// session gets a *duplicate* through `SCM_RIGHTS`, so dropping this
    /// closes the daemon's copy and nothing else. Nobody else may close this
    /// descriptor: that is the whole reason it is an `OwnedFd` and not the
    /// `RawFd` the accept loop used to pass around.
    fd: OwnedFd,
    user: UserInfo,
    peer: String,
    slot: Slot,
}

/// A connection the pool would not take, handed back so the client can be told
/// why before it is closed.
pub struct Busy {
    /// The client's socket, still open and now the caller's again.
    pub fd: OwnedFd,
    /// What to tell the client, and what to log.
    pub reason: String,
}

/// Threads that perform handoffs, so the accept loop never does.
pub struct HandoffPool {
    /// `None` once [`HandoffPool::shutdown`] has run, which is what makes the
    /// workers' `recv` return and lets them be joined.
    jobs: Option<Sender<Job>>,
    workers: Vec<JoinHandle<()>>,
    admission: Arc<Admission>,
    /// Kept so `shutdown` can tell the manager, which is where a worker
    /// part-way through a handoff looks: the flag has to be visible after the
    /// start latch as well as before the job, and only the manager is there.
    manager: Arc<SessionManager>,
}

impl HandoffPool {
    /// Start the workers.
    ///
    /// A thread that cannot be started is fatal rather than tolerated: a pool
    /// short of workers looks exactly like a healthy daemon until the day it
    /// is busy, and a host that cannot make sixteen threads has told us
    /// something worth refusing to start over.
    pub fn new(manager: Arc<SessionManager>) -> std::io::Result<Self> {
        let (jobs, rx) = bounded::<Job>(HANDOFF_QUEUE);
        let mut workers = Vec::with_capacity(HANDOFF_WORKERS);
        for i in 0..HANDOFF_WORKERS {
            let rx = rx.clone();
            let manager = Arc::clone(&manager);
            // On failure `jobs` and this `rx` drop with the error, so any
            // worker already started sees the channel disconnect and exits.
            let handle = std::thread::Builder::new()
                .name(format!("handoff-{i}"))
                .spawn(move || worker(&rx, &manager))?;
            workers.push(handle);
        }
        Ok(Self {
            jobs: Some(jobs),
            workers,
            admission: Arc::new(Admission::default()),
            manager,
        })
    }

    /// Queue a connection for a worker.
    ///
    /// The descriptor comes back inside [`Busy`] when the pool will not take
    /// it, because the accept loop still owes that client an explanation and
    /// a close, and neither can happen if the fd has been swallowed.
    pub fn submit(&self, fd: OwnedFd, user: UserInfo, peer: String) -> Result<(), Busy> {
        let Some(jobs) = self.jobs.as_ref() else {
            return Err(Busy {
                fd,
                reason: "the daemon is shutting down".to_string(),
            });
        };
        let Some(slot) = Admission::take(&self.admission, user.uid, PER_UID_IN_FLIGHT) else {
            let reason = format!(
                "{} already has {PER_UID_IN_FLIGHT} connections waiting for a session",
                user.name
            );
            return Err(Busy { fd, reason });
        };
        let job = Job {
            fd,
            user,
            peer,
            slot,
        };
        match jobs.try_send(job) {
            Ok(()) => Ok(()),
            // The job comes back whole, so its `Slot` is released by the drop
            // at the end of this arm and the refusal does not permanently cost
            // this uid a place.
            Err(TrySendError::Full(job)) => Err(Busy {
                fd: job.fd,
                reason: "the server is busy starting sessions".to_string(),
            }),
            Err(TrySendError::Disconnected(job)) => Err(Busy {
                fd: job.fd,
                reason: "the daemon is shutting down".to_string(),
            }),
        }
    }

    /// Stop taking work and wait for the workers.
    ///
    /// Queued jobs are discarded rather than run -- dropping one closes the
    /// client socket, which is what that client's own timeout would have done
    /// a moment later anyway -- and a job that had left the queue but was
    /// still waiting on its uid's start latch is refused once the latch is
    /// its own, so this waits for at most one handoff per worker. That is
    /// still up to `COLD_START_REPLY_TIMEOUT` plus a supervisor's
    /// [`SUPERVISOR_EXIT_GRACE`], which is the same wait a SIGTERM arriving
    /// mid-handoff has always had, and inside systemd's default stop timeout
    /// of ninety seconds. Before the latch re-check it was not: a worker
    /// parked behind another's cold start ran its own in full afterwards,
    /// and two cold starts is past that timeout.
    pub fn shutdown(&mut self) {
        self.manager.begin_shutdown();
        // Dropping the only sender is what ends the workers' `recv` loop.
        drop(self.jobs.take());
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker(jobs: &Receiver<Job>, manager: &SessionManager) {
    while let Ok(job) = jobs.recv() {
        if manager.is_stopping() {
            // Dropping the job closes the client's socket and releases its
            // place. Starting a desktop for someone the daemon is about to
            // stop serving would only leave a session behind.
            continue;
        }
        run_job(job, manager);
    }
}

fn run_job(job: Job, manager: &SessionManager) {
    let Job {
        fd,
        user,
        peer,
        slot,
    } = job;
    match manager.handoff(&user, fd.as_fd(), &peer) {
        Ok(_) => {}
        Err(e) => {
            log::error!("session handoff for {} failed: {e:#}", user.name);
            send_rejection(
                fd.as_fd(),
                reject::SESSION_FAILED,
                &format!("could not start a session: {e:#}"),
            );
        }
    }
    // The daemon's copy of the client socket closes here -- the session holds
    // a duplicate of its own -- and the place this connection held in the pool
    // goes back. Both are explicit because both used to be somebody else's
    // problem: the accept loop closed a raw descriptor by hand.
    drop(fd);
    drop(slot);
}

/// Stop a supervisor politely, and only then insistently.
///
/// SIGKILL on its own is wrong here. The supervisor holds the PAM session open
/// and runs `pam_close_session` only once the session it forwarded SIGTERM to
/// has exited, so killing it outright leaked a logind session on every failed
/// attempt -- and, because `lynxrdp-session` had no parent-death link, the
/// whole desktop with it. `grace` has to cover the supervisor's own bounded
/// wait for that exit, which is what [`SUPERVISOR_EXIT_GRACE`] is.
fn terminate(child: &mut Child, grace: Duration) {
    terminate_all(vec![child], grace);
}

/// [`terminate`] for several supervisors at once, sharing one grace period.
fn terminate_all(mut children: Vec<&mut Child>, grace: Duration) {
    for child in &children {
        // SAFETY: signalling our own child.
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
    }
    let deadline = Instant::now() + grace;
    loop {
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        if children.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    for child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
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

/// Why a handoff to an existing session's socket did not go through.
///
/// The two are told apart because they call for opposite responses. A socket
/// nothing is bound to belongs to a session that has exited, and its name is
/// free to take. A socket that is bound but did not answer belongs to a
/// session that is still there -- stopped by its owner, or on a host too busy
/// to schedule it within the timeout -- and taking its name from it strands
/// a running desktop. `handoff` used to treat both as the first.
#[derive(Debug)]
enum HandoffFailure {
    /// Nothing is listening: the connect was refused, or the file is gone.
    Dead(anyhow::Error),
    /// Something is listening and did not take the connection: no reply
    /// within the timeout, a refusal, or a connection closed unanswered.
    Unusable(anyhow::Error),
}

impl HandoffFailure {
    fn into_error(self) -> anyhow::Error {
        match self {
            HandoffFailure::Dead(e) | HandoffFailure::Unusable(e) => e,
        }
    }
}

fn try_handoff(
    socket_path: &Path,
    user: &UserInfo,
    client_fd: BorrowedFd<'_>,
    peer: &str,
    timeout: Duration,
) -> std::result::Result<(), HandoffFailure> {
    let control = match UnixStream::connect(socket_path) {
        Ok(c) => c,
        Err(e) => {
            // ECONNREFUSED is what a socket file with no listener behind it
            // answers -- and what a plain file at that name answers too --
            // and ENOENT is one that has been unlinked. Anything else says
            // nothing about whether a session is there, and is treated as if
            // one were: the error that fails closed.
            let dead = matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            );
            let e =
                anyhow::Error::new(e).context(format!("connecting to {}", socket_path.display()));
            return Err(if dead {
                HandoffFailure::Dead(e)
            } else {
                HandoffFailure::Unusable(e)
            });
        }
    };
    deliver(&control, socket_path, user, client_fd, peer, timeout).map_err(HandoffFailure::Unusable)
}

/// The half of [`try_handoff`] after the connect: everything that fails here
/// fails with a live session on the other end.
fn deliver(
    control: &UnixStream,
    socket_path: &Path,
    user: &UserInfo,
    client_fd: BorrowedFd<'_>,
    peer: &str,
    timeout: Duration,
) -> Result<()> {
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
    let owner = crate::peer::unix_peer(control)
        .with_context(|| format!("identifying the owner of {}", socket_path.display()))?;
    let own = crate::peer::own_uid();
    if owner.uid != own && owner.uid != 0 {
        bail!(
            "{} is served by uid {}, not by uid {own} or root",
            socket_path.display(),
            owner.uid
        );
    }
    let h = Handoff {
        uid: user.uid,
        username: user.name.clone(),
        peer: peer.to_string(),
    };
    // The one place the borrow is flattened to a number, because SCM_RIGHTS
    // deals in descriptors and nothing else. The kernel gives the session its
    // own descriptor for the same open file; ours stays ours, and the worker
    // closes it once when the job ends.
    let reply = send_handoff(control, &h, client_fd.as_raw_fd(), timeout).map_err(|e| {
        match e.kind() {
            // The socket's timeout surfaces as EAGAIN, which nobody reading a
            // log or a rejection would recognise as "it never answered".
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                anyhow!("no reply within {timeout:?}")
            }
            _ => anyhow::Error::new(e).context("sending the handoff"),
        }
    })?;
    match reply {
        Reply::Accepted => Ok(()),
        Reply::Refused => bail!("session refused the handoff"),
    }
}

/// Everything in `[session]` that a session process is told about.
///
/// This list is the whole of a session's configuration: the process reads no
/// file of its own, so a key that is not turned into an argument here is a key
/// an operator can set to no effect at all. `max_in_flight_auto` was one for a
/// while, which is why the mapping now sits on its own with a test against it.
fn session_argv(s: &SessionConfig, session_id: u64) -> Vec<String> {
    let mut args: Vec<String> = vec![
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
    // The session's own switch is the negative one -- adaptation is on unless
    // it is given -- so this appears only when an operator has turned the
    // window down to a fixed size, and it carries no value of its own.
    if !s.max_in_flight_auto {
        args.push("--no-auto-in-flight".into());
    }
    for a in &s.xserver_args {
        args.push("--xserver-arg".into());
        args.push(a.clone());
    }
    args
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

    /// The fairness property the pool exists for, at the one layer that can be
    /// asserted without two real uids: a uid whose handoff is in progress
    /// holds up only its own next connection.
    ///
    /// The daemon-level version of this cannot be tested in CI at all --
    /// `--allow-non-root` serves the invoking uid and no other, so an
    /// integration test can never produce a second user to be starved.
    #[test]
    fn a_uid_being_started_holds_up_only_itself() {
        let starts = Arc::new(Starts::default());
        let held = starts.acquire(1000);
        let (tx, rx) = bounded::<u32>(2);

        let same_uid = {
            let starts = Arc::clone(&starts);
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _claim = starts.acquire(1000);
                tx.send(1000).unwrap();
            })
        };
        let other_uid = {
            let starts = Arc::clone(&starts);
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _claim = starts.acquire(1001);
                tx.send(1001).unwrap();
            })
        };

        // The other user goes straight through. This is the whole point: on
        // the old accept loop they would have waited out uid 1000's cold start.
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 1001);
        // The same user's second connection waits, which is also the point:
        // letting it past is what orphaned a supervisor.
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
        drop(held);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 1000);

        same_uid.join().unwrap();
        other_uid.join().unwrap();
        // The set is empty again, so it is bounded by handoffs in flight and
        // not by every uid that has ever connected.
        assert!(lock(&starts.busy).is_empty());
    }

    /// An exited supervisor whose `Child` has already been waited for, so
    /// `try_wait` is deterministic rather than a race with the scheduler.
    fn exited_child() -> Child {
        let mut c = Command::new("/bin/sh")
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sh");
        c.wait().expect("wait for /bin/sh");
        c
    }

    /// A manager with no directories to prepare, built by hand so the test
    /// does not depend on `ensure_owned_dir` and the ownership of a temporary
    /// directory.
    fn manager_at(sessions_dir: &Path) -> SessionManager {
        SessionManager {
            cfg: Config::default(),
            inner: Mutex::new(Inner {
                sessions: HashMap::new(),
                last_probe: Instant::now(),
            }),
            starts: Starts::default(),
            sessions_dir: sessions_dir.to_path_buf(),
            log_dir: sessions_dir.join("log"),
            stopping: AtomicBool::new(false),
        }
    }

    fn user(uid: u32, name: &str) -> UserInfo {
        UserInfo {
            uid,
            // SAFETY: getgid has no preconditions.
            gid: unsafe { libc::getgid() },
            name: name.into(),
            home: "/".into(),
            shell: "/bin/sh".into(),
        }
    }

    /// The failure a dead socket produces is told apart from the one a live
    /// but silent session produces, because the two call for opposite
    /// responses: the first name is free to take, the second is not.
    #[test]
    fn a_dead_socket_and_a_silent_session_are_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let (client, _other) = UnixStream::pair().unwrap();
        let me = user(crate::peer::own_uid(), "me");
        let short = Duration::from_millis(300);
        let attempt = |path: &Path| try_handoff(path, &me, client.as_fd(), "p", short);

        // Nothing at the name, a plain file at the name, and a socket whose
        // listener has gone while the file stayed: all three are dead.
        let gone = dir.path().join("gone.sock");
        assert!(matches!(attempt(&gone), Err(HandoffFailure::Dead(_))));
        let plain = dir.path().join("plain.sock");
        fs::write(&plain, b"").unwrap();
        assert!(matches!(attempt(&plain), Err(HandoffFailure::Dead(_))));
        let closed = dir.path().join("closed.sock");
        drop(UnixListener::bind(&closed).unwrap());
        assert!(closed.exists());
        assert!(matches!(attempt(&closed), Err(HandoffFailure::Dead(_))));

        // A listener that never accepts: the connect succeeds and the reply
        // never comes, which is what a stopped session looks like.
        let silent = dir.path().join("silent.sock");
        let _listener = UnixListener::bind(&silent).unwrap();
        let err = attempt(&silent).unwrap_err();
        assert!(matches!(err, HandoffFailure::Unusable(_)), "{err:?}");
        let text = format!("{:#}", err.into_error());
        assert!(text.contains("no reply"), "{text}");
    }

    /// A session this daemon did not start, and so cannot signal, is left
    /// exactly as it is when it fails to answer: socket in place, record in
    /// place, and no second desktop started beside it. Unlinking it was how
    /// a daemon restart followed by ten busy seconds lost a user's desktop.
    #[test]
    fn an_adopted_session_that_does_not_answer_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager_at(dir.path());
        let me = user(crate::peer::own_uid(), "me");
        let socket = dir.path().join(format!("{}.sock", me.uid));
        let _listener = UnixListener::bind(&socket).unwrap();
        lock(&mgr.inner).sessions.insert(
            me.uid,
            SessionRecord {
                supervisor: None,
                socket_path: socket.clone(),
                session_id: 0,
                username: me.name.clone(),
                started: Instant::now(),
            },
        );
        let (client, _other) = UnixStream::pair().unwrap();
        let short = Duration::from_millis(300);
        let err = mgr
            .handoff_with(&me, client.as_fd(), "p", short)
            .unwrap_err();
        assert!(format!("{err:#}").contains("still running"), "{err:#}");
        assert!(socket.exists(), "the adopted session's socket was unlinked");
        assert!(
            UnixStream::connect(&socket).is_ok(),
            "the listener was taken away"
        );
        assert_eq!(mgr.count(), 1, "the record was dropped");

        // The same with no record at all -- the first connection after a
        // restart, before anything has been adopted -- because what matters
        // is that there is no supervisor to signal, not whether the session
        // has been written down yet.
        lock(&mgr.inner).sessions.clear();
        let err = mgr
            .handoff_with(&me, client.as_fd(), "p", short)
            .unwrap_err();
        assert!(format!("{err:#}").contains("still running"), "{err:#}");
        assert!(socket.exists());
        assert!(UnixStream::connect(&socket).is_ok());
    }

    /// A connection that waited on the start latch behind another for the
    /// same uid is refused, not started, when the daemon was told to stop
    /// while it waited. Before the re-check it ran its cold start in full,
    /// which put shutdown past systemd's stop timeout.
    #[test]
    fn a_handoff_that_waited_on_the_latch_refuses_once_shutdown_began() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(manager_at(dir.path()));
        let me = user(crate::peer::own_uid(), "me");
        let held = mgr.starts.acquire(me.uid);
        let waiter = {
            let mgr = Arc::clone(&mgr);
            let me = me.clone();
            std::thread::spawn(move || {
                let (client, _other) = UnixStream::pair().unwrap();
                mgr.handoff(&me, client.as_fd(), "p")
                    .map(|_| ())
                    .map_err(|e| format!("{e:#}"))
            })
        };
        // Told to stop while (or before) it waits, and released only after.
        mgr.begin_shutdown();
        drop(held);
        let err = waiter.join().unwrap().unwrap_err();
        assert!(err.contains("shutting down"), "{err}");
        assert!(
            !dir.path().join(format!("{}.sock", me.uid)).exists(),
            "a session was spawned during shutdown"
        );
        assert_eq!(mgr.count(), 0);
    }

    /// Root appending to `<user>.log` must not follow a symlink planted at
    /// that name, and must not change the ownership of whatever it points at.
    #[test]
    fn the_session_log_is_not_opened_through_a_symlink() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("not-a-log");
        fs::write(&target, b"precious").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("alice.log")).unwrap();
        let err = open_session_log(dir.path(), &user(1000, "alice")).unwrap_err();
        assert!(format!("{err:#}").contains("alice.log"), "{err:#}");
        assert_eq!(fs::read(&target).unwrap(), b"precious");

        // An ordinary log, or none yet, is opened for appending as before.
        let bob = user(1001, "bob");
        let (path, mut log) = open_session_log(dir.path(), &bob).unwrap();
        log.write_all(b"one\n").unwrap();
        drop(log);
        let (_, mut log) = open_session_log(dir.path(), &bob).unwrap();
        log.write_all(b"two\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"one\ntwo\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn add_ended_session(mgr: &SessionManager, uid: u32, socket_path: &Path) {
        fs::write(socket_path, b"").expect("stand-in for a control socket");
        lock(&mgr.inner).sessions.insert(
            uid,
            SessionRecord {
                supervisor: Some(exited_child()),
                socket_path: socket_path.to_path_buf(),
                session_id: 1,
                username: format!("user{uid}"),
                started: Instant::now(),
            },
        );
    }

    /// `reap` may forget a dead session at any time, but it may only take the
    /// *name* away when nothing is putting a new socket there.
    ///
    /// `spawn` unlinks and binds this exact path with the map unlocked, so the
    /// two are made exclusive by the uid's start latch and by nothing else.
    /// Before the pool existed, reaping and handing off were the same thread
    /// and could not race; now they can, and an unguarded unlink takes the
    /// name from a session a worker bound seconds ago -- a live X server that
    /// nothing can reach again, which is the failure the latch exists to
    /// prevent.
    #[test]
    fn reap_does_not_unlink_a_socket_while_its_uid_is_being_started() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager_at(dir.path());

        let busy = dir.path().join("1000.sock");
        add_ended_session(&mgr, 1000, &busy);
        let claim = mgr.starts.acquire(1000);
        mgr.reap();
        assert!(
            busy.exists(),
            "reap unlinked a path a handoff for uid 1000 may have just bound"
        );
        // The record still goes, so `count()` and the heartbeat stay honest:
        // it is only the unlink that waits.
        assert_eq!(mgr.count(), 0);
        drop(claim);

        // And with nobody starting that uid, the stale name is still cleaned
        // up -- the guard must not have turned the reaper off.
        let free = dir.path().join("1001.sock");
        add_ended_session(&mgr, 1001, &free);
        mgr.reap();
        assert!(!free.exists(), "reap left a stale socket for an idle uid");
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn one_uid_cannot_take_the_whole_pool() {
        let admission = Arc::new(Admission::default());
        let first = Admission::take(&admission, 1000, 2).expect("first place");
        let second = Admission::take(&admission, 1000, 2).expect("second place");
        // Refused rather than queued. A user who has stopped their own session
        // would otherwise park a worker for every connection they open.
        assert!(Admission::take(&admission, 1000, 2).is_none());
        // Another user is not affected by any of it.
        let other = Admission::take(&admission, 1001, 2).expect("a different uid");
        drop(first);
        let reused = Admission::take(&admission, 1000, 2).expect("the freed place");

        drop(second);
        drop(other);
        drop(reused);
        // Entries go when they reach zero, so this map is bounded by who is
        // connecting now rather than by everyone who ever has.
        assert!(lock(&admission.in_flight).is_empty());
    }

    /// `max_in_flight_auto = false` has to reach the session as
    /// `--no-auto-in-flight`, because that argument is the only thing a
    /// session ever hears about the setting.
    ///
    /// It was parsed, validated and offered in the packaged template, and then
    /// dropped here, so the only session that honoured it was one an operator
    /// ran by hand. Anything else added to `[session]` and forgotten in
    /// `session_argv` fails the same silent way.
    #[test]
    fn turning_off_the_adaptive_window_reaches_the_session() {
        let mut cfg = Config::default();
        assert!(cfg.session.max_in_flight_auto);
        let on = session_argv(&cfg.session, 1);
        assert!(!on.iter().any(|a| a == "--no-auto-in-flight"));

        cfg.session.max_in_flight_auto = false;
        let off = session_argv(&cfg.session, 1);
        assert!(
            off.iter().any(|a| a == "--no-auto-in-flight"),
            "the daemon dropped session.max_in_flight_auto: {off:?}"
        );
        // A bare switch and not a pair: every other setting here is `--flag
        // value`, and giving this one a value of its own would fail the
        // session's parse rather than configure anything.
        assert_eq!(off.len(), on.len() + 1);
    }
}
