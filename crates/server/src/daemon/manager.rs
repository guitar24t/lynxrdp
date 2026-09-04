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

use anyhow::{bail, Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use lynxrdp_proto::message::reject;

use super::send_rejection;
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

/// How long a supervisor gets to shut down cleanly when a handoff has failed.
///
/// Deliberately shorter than `stop_all`'s five seconds. The original reason --
/// that this ran on the daemon's single accept loop -- no longer holds, but it
/// still runs on one of [`HANDOFF_WORKERS`] threads with a user waiting at the
/// end of it, and a supervisor that has not answered SIGTERM within a second
/// is not going to.
const HANDOFF_TERMINATE_GRACE: Duration = Duration::from_secs(1);

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
        // Everything below is serialised for this uid and for no other. Two
        // connections from one user arriving together would otherwise both
        // find no socket, both spawn, and the second spawn's `remove_file` +
        // `bind` would orphan the first supervisor with the user's desktop
        // still inside it.
        let _starting = self.starts.acquire(user.uid);
        self.reap();
        let socket_path = self.sessions_dir.join(format!("{}.sock", user.uid));
        // Try an existing session first (ours, or one surviving a daemon restart).
        if socket_path.exists() {
            match try_handoff(
                &socket_path,
                user,
                client_fd,
                peer,
                ESTABLISHED_REPLY_TIMEOUT,
            ) {
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
                Err(e) => {
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
            Err(e) => {
                // Safe to remove by uid alone: the start latch means no other
                // thread can have inserted a record for this user, and if
                // `reap` beat us to it there is nothing left to stop.
                let failed = {
                    let mut inner = lock(&self.inner);
                    inner.sessions.remove(&user.uid)
                };
                if let Some(mut rec) = failed {
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
    ///
    /// Run [`HandoffPool::shutdown`] first. A worker part-way through a spawn
    /// would otherwise insert its record after the map had been drained, and
    /// that session would outlive the daemon it was asked to end with it.
    pub fn stop_all(&self) {
        let records: Vec<(u32, SessionRecord)> = {
            let mut inner = lock(&self.inner);
            inner.sessions.drain().collect()
        };
        for (uid, mut rec) in records {
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
    stopping: Arc<AtomicBool>,
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
        let stopping = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(HANDOFF_WORKERS);
        for i in 0..HANDOFF_WORKERS {
            let rx = rx.clone();
            let manager = Arc::clone(&manager);
            let stopping = Arc::clone(&stopping);
            // On failure `jobs` and this `rx` drop with the error, so any
            // worker already started sees the channel disconnect and exits.
            let handle = std::thread::Builder::new()
                .name(format!("handoff-{i}"))
                .spawn(move || worker(&rx, &manager, &stopping))?;
            workers.push(handle);
        }
        Ok(Self {
            jobs: Some(jobs),
            workers,
            admission: Arc::new(Admission::default()),
            stopping,
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
    /// a moment later anyway -- so this waits for at most one handoff per
    /// worker. That is still up to `COLD_START_REPLY_TIMEOUT`, which is the
    /// same wait a SIGTERM arriving mid-handoff has always had, and well
    /// inside systemd's default stop timeout.
    pub fn shutdown(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Dropping the only sender is what ends the workers' `recv` loop.
        drop(self.jobs.take());
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker(jobs: &Receiver<Job>, manager: &SessionManager, stopping: &AtomicBool) {
    while let Ok(job) = jobs.recv() {
        if stopping.load(Ordering::SeqCst) {
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
    client_fd: BorrowedFd<'_>,
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
    let h = Handoff {
        uid: user.uid,
        username: user.name.clone(),
        peer: peer.to_string(),
    };
    // The one place the borrow is flattened to a number, because SCM_RIGHTS
    // deals in descriptors and nothing else. The kernel gives the session its
    // own descriptor for the same open file; ours stays ours, and the worker
    // closes it once when the job ends.
    match send_handoff(&control, &h, client_fd.as_raw_fd(), timeout)? {
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
        }
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
}
