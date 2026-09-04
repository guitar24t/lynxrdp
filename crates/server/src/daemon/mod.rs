//! The privileged `lynxrdpd` daemon.
//!
//! ```text
//!  ssh -L 3390:127.0.0.1:3390 user@host        (client side)
//!        │
//!        ▼ loopback TCP, socket owned by `user`
//!  lynxrdpd  ── identifies uid via /proc/net/tcp ── access policy
//!        │
//!        ▼ handoff worker pool
//!        ├─ existing session?  ── SCM_RIGHTS handoff ──▶ lynxrdp-session (user)
//!        └─ else: lynxrdpd --supervise (root, PAM) ──▶ lynxrdp-session (user)
//! ```
//!
//! Only the top half of that runs on the listening thread. Accepting,
//! identifying the peer and applying the access policy are all microseconds,
//! and the `/proc/net/tcp` lookup has to happen there in any case -- it needs
//! the peer's socket to still be in the kernel's table, which it will not be
//! by the time a worker gets round to it. Everything below the pool is slow
//! and unbounded: see [`manager`] for what one stalled session used to do to
//! everybody else's connection.

pub mod access;
pub mod manager;
pub mod pam;
pub mod supervisor;
pub mod users;

use std::os::unix::io::{AsRawFd, BorrowedFd, RawFd};
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

/// Best-effort: tell the client why it was refused. The caller still owns the
/// descriptor and still has to close it.
///
/// The descriptor is borrowed rather than raw, and rather than duplicated into
/// a `File` as this used to do, because refusals are now sent from two places
/// -- the accept loop and a pool worker -- and "who closes this" has to have
/// one answer no matter which. It is the owner, and it is never this function.
///
/// The send is non-blocking. A rejection is advisory: the close that follows
/// says the same thing, and on the accept loop a peer that has stopped reading
/// must not be able to hold up the next user's connection while it is told
/// something it is not listening to. The frame is around a hundred bytes into
/// an empty socket buffer, so a client that is actually there receives it.
pub fn send_rejection(fd: BorrowedFd<'_>, code: u16, reason: &str) {
    let mut buf = Vec::new();
    frame_message(
        &Message::Rejected {
            code,
            reason: reason.to_string(),
        },
        &mut buf,
    );
    let mut sent = 0usize;
    while sent < buf.len() {
        // SAFETY: `fd` is a live borrowed socket and the slice is in bounds.
        let n = unsafe {
            libc::send(
                fd.as_raw_fd(),
                buf[sent..].as_ptr() as *const libc::c_void,
                buf.len() - sent,
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            // EAGAIN, EPIPE, a reset: there is nothing useful left to try.
            return;
        }
        if n == 0 {
            return;
        }
        sent += n as usize;
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
