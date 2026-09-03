//! Session lifecycle: find a user's running session or start one, and hand
//! client connections to it.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
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
        for d in [&runtime_dir, &sessions_dir, &log_dir] {
            if !d.is_dir() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(d)
                    .with_context(|| format!("creating {}", d.display()))?;
            }
        }
        // The sessions directory must not be traversable by others: whoever can
        // connect to a session socket can hand it arbitrary connections.
        fs::set_permissions(
            &sessions_dir,
            <fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
        )?;
        Ok(Self {
            cfg,
            sessions: HashMap::new(),
            sessions_dir,
            log_dir,
        })
    }

    /// Directory holding session control sockets.
    pub fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    /// Reap supervisors that have exited and forget their sessions.
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
        for uid in gone {
            if let Some(rec) = self.sessions.remove(&uid) {
                let _ = fs::remove_file(&rec.socket_path);
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
                            let _ = c.kill();
                            let _ = c.wait();
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
                        let _ = c.kill();
                        let _ = c.wait();
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
    pub fn stop_all(&mut self) {
        for (_, mut rec) in self.sessions.drain() {
            if let Some(c) = rec.supervisor.as_mut() {
                // SAFETY: signalling our own child.
                unsafe {
                    libc::kill(c.id() as i32, libc::SIGTERM);
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = c.try_wait() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = c.kill();
                let _ = c.wait();
            }
            let _ = fs::remove_file(&rec.socket_path);
        }
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
    control.set_read_timeout(Some(timeout))?;
    let h = Handoff {
        uid: user.uid,
        username: user.name.clone(),
        peer: peer.to_string(),
    };
    match send_handoff_with_timeout(&control, &h, client_fd, timeout)? {
        Reply::Accepted => Ok(()),
        Reply::Refused => bail!("session refused the handoff"),
    }
}

fn send_handoff_with_timeout(
    control: &UnixStream,
    h: &Handoff,
    fd: RawFd,
    timeout: Duration,
) -> Result<Reply> {
    control.set_read_timeout(Some(timeout))?;
    let r = send_handoff(control, h, fd)?;
    Ok(r)
}

fn new_session_id() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    (t ^ (pid << 48)) & 0x7fff_ffff_ffff_ffff
}
