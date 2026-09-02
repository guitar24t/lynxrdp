//! The privileged `lynxrdpd` daemon.
//!
//! ```text
//!  ssh -L 3390:127.0.0.1:3390 user@host        (client side)
//!        │
//!        ▼ loopback TCP, socket owned by `user`
//!  lynxrdpd  ── identifies uid via /proc/net/tcp ── access policy
//!        │
//!        ├─ existing session?  ── SCM_RIGHTS handoff ──▶ lynxrdp-session (user)
//!        └─ else: lynxrdpd --supervise (root, PAM) ──▶ lynxrdp-session (user)
//! ```

pub mod access;
pub mod manager;
pub mod pam;
pub mod supervisor;
pub mod users;

use std::io::Write;
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

use lynxrdp_proto::frame::frame_message;
use lynxrdp_proto::message::reject;
use lynxrdp_proto::Message;

use crate::config::Config;
use crate::peer::PeerIdentity;

/// Result of evaluating a connection.
#[derive(Debug)]
pub enum Decision {
    /// Hand the connection to this user's session.
    Accept(users::UserInfo),
    /// Refuse with this reason.
    Reject(u16, String),
}

/// Decide what to do with a connection from `peer`.
pub fn decide(cfg: &Config, peer: Option<PeerIdentity>) -> Decision {
    let Some(peer) = peer else {
        return Decision::Reject(
            reject::UNAUTHORIZED,
            "could not identify the local user behind the connection".into(),
        );
    };
    let user = match users::user_by_uid(peer.uid) {
        Ok(u) => u,
        Err(e) => {
            return Decision::Reject(
                reject::UNAUTHORIZED,
                format!("unknown uid {}: {e}", peer.uid),
            )
        }
    };
    let groups = users::groups_of(&user);
    match access::check(&cfg.access, user.uid, &user.name, &groups) {
        Ok(()) => Decision::Accept(user),
        Err(reason) => Decision::Reject(reject::UNAUTHORIZED, reason),
    }
}

/// Best-effort: tell the client why it was refused, then close.
pub fn send_rejection(fd: RawFd, code: u16, reason: &str) {
    let mut buf = Vec::new();
    frame_message(
        &Message::Rejected {
            code,
            reason: reason.to_string(),
        },
        &mut buf,
    );
    // SAFETY: we own the fd; a short write timeout keeps a stuck peer from
    // blocking the daemon.
    unsafe {
        let tv = libc::timeval {
            tv_sec: 2,
            tv_usec: 0,
        };
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_SNDTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
        let mut f = std::fs::File::from(std::os::unix::io::OwnedFd::from_raw_fd_dup(fd));
        let _ = f.write_all(&buf);
        let _ = f.flush();
    }
}

trait DupFd {
    unsafe fn from_raw_fd_dup(fd: RawFd) -> Self;
}

impl DupFd for std::os::unix::io::OwnedFd {
    unsafe fn from_raw_fd_dup(fd: RawFd) -> Self {
        use std::os::unix::io::FromRawFd;
        let d = libc::dup(fd);
        Self::from_raw_fd(d)
    }
}

/// Wait up to `timeout` for a listener fd to become readable. Returns the
/// index of the ready listener.
pub fn poll_listeners(fds: &[RawFd], timeout: Duration) -> std::io::Result<Option<usize>> {
    let mut pfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    // SAFETY: pfds is valid for its length.
    let rc = unsafe {
        libc::poll(
            pfds.as_mut_ptr(),
            pfds.len() as libc::nfds_t,
            timeout.as_millis() as libc::c_int,
        )
    };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(e);
    }
    Ok(pfds
        .iter()
        .position(|p| p.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0))
}

#[allow(dead_code)]
fn _uses(_: &dyn AsRawFd) {}
