//! Protocol between `lynxrdpd` and `lynxrdp-session` for handing a client
//! connection to the session process.
//!
//! The daemon connects to the session's control socket, sends one
//! [`Handoff`] record together with the client's socket (`SCM_RIGHTS`) and
//! waits for a one-byte reply. Each handoff uses a fresh connection.

use std::io::{self, Read, Write};
use std::os::unix::io::RawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use lynxrdp_proto::wire::{Reader, Writer};

use crate::fdpass::{recv_with_fd, send_with_fd};

/// Magic prefix guarding against stray connections.
const MAGIC: &[u8; 4] = b"LXHO";

/// Request to take over a client connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    /// Uid the daemon identified for the peer.
    pub uid: u32,
    /// User name the daemon resolved for the peer.
    pub username: String,
    /// Description of the peer for logging (e.g. `127.0.0.1:41234`).
    pub peer: String,
}

/// Reply from the session process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Reply {
    /// The session took over the connection.
    Accepted = 1,
    /// The session refused (e.g. uid mismatch).
    Refused = 2,
}

impl Handoff {
    /// Serialise.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.raw(MAGIC);
        w.u32(self.uid);
        w.string(&self.username);
        w.string(&self.peer);
        w.into_inner()
    }

    /// Parse.
    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut r = Reader::new(bytes);
        let bad =
            |e: lynxrdp_proto::wire::DecodeError| io::Error::new(io::ErrorKind::InvalidData, e);
        if r.raw(4).map_err(bad)? != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad handoff magic",
            ));
        }
        let uid = r.u32().map_err(bad)?;
        let username = r.string().map_err(bad)?;
        let peer = r.string().map_err(bad)?;
        r.finish().map_err(bad)?;
        Ok(Handoff {
            uid,
            username,
            peer,
        })
    }
}

/// Daemon side: send the client fd to the session and wait for the reply.
///
/// `reply_timeout` is how long the session gets to answer. It is a parameter
/// rather than a constant because the two callers have genuinely different
/// budgets: an established session replies immediately, while a cold start has
/// to bring up Xvfb first. This function used to hard-code ten seconds and
/// silently overwrite whatever the caller had set on the socket, so the
/// forty-five seconds the manager budgets for a cold start became ten -- less
/// than the session's own twenty-second displayfd wait plus ten-second connect
/// allowance, which meant a perfectly healthy session on a loaded host was
/// declared failed and killed.
pub fn send_handoff(
    control: &UnixStream,
    handoff: &Handoff,
    client_fd: RawFd,
    reply_timeout: Duration,
) -> io::Result<Reply> {
    control.set_write_timeout(Some(Duration::from_secs(5)))?;
    control.set_read_timeout(Some(reply_timeout))?;
    send_with_fd(control, &handoff.encode(), client_fd)?;
    let mut b = [0u8; 1];
    (&*control).read_exact(&mut b)?;
    match b[0] {
        1 => Ok(Reply::Accepted),
        2 => Ok(Reply::Refused),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad reply {other}"),
        )),
    }
}

/// Session side: receive a handoff and the client fd.
pub fn recv_handoff(control: &UnixStream) -> io::Result<(Handoff, std::os::unix::io::OwnedFd)> {
    control.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = vec![0u8; 4096];
    let (n, fd) = recv_with_fd(control, &mut buf)?;
    let handoff = Handoff::decode(&buf[..n])?;
    let fd = fd.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "handoff without fd"))?;
    Ok((handoff, fd))
}

/// Session side: answer a handoff.
pub fn send_reply(control: &UnixStream, reply: Reply) -> io::Result<()> {
    (&*control).write_all(&[reply as u8])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::io::AsRawFd;

    #[test]
    fn handoff_roundtrip() {
        let h = Handoff {
            uid: 1000,
            username: "alice".into(),
            peer: "127.0.0.1:5".into(),
        };
        assert_eq!(Handoff::decode(&h.encode()).unwrap(), h);
        assert!(Handoff::decode(b"nope").is_err());
        let mut bad = h.encode();
        bad[0] = b'X';
        assert!(Handoff::decode(&bad).is_err());
    }

    #[test]
    fn end_to_end_handoff() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        let (daemon, session) = UnixStream::pair().unwrap();
        let h = Handoff {
            uid: 7,
            username: "u".into(),
            peer: "p".into(),
        };
        let t = std::thread::spawn(move || {
            let (got, fd) = recv_handoff(&session).unwrap();
            assert_eq!(got.uid, 7);
            assert!(fd.as_raw_fd() >= 0);
            send_reply(&session, Reply::Accepted).unwrap();
        });
        let reply =
            send_handoff(&daemon, &h, server_side.as_raw_fd(), Duration::from_secs(5)).unwrap();
        assert_eq!(reply, Reply::Accepted);
        t.join().unwrap();
    }

    /// The caller's reply timeout is the one that applies.
    ///
    /// `send_handoff` used to set its own ten seconds on the socket, silently
    /// discarding whatever the caller had chosen. The manager budgets
    /// forty-five seconds for a cold start -- more than the session's own
    /// twenty-second displayfd wait plus ten-second connect allowance -- and
    /// got ten, so a healthy session on a loaded host was declared failed,
    /// killed, and (before the parent-death link) orphaned.
    #[test]
    fn the_callers_reply_timeout_is_honoured() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        // `session` is held open and never answers, so the only thing that can
        // end this call is the timeout.
        let (daemon, _session) = UnixStream::pair().unwrap();
        let h = Handoff {
            uid: 7,
            username: "u".into(),
            peer: "p".into(),
        };
        let started = std::time::Instant::now();
        let err = send_handoff(
            &daemon,
            &h,
            server_side.as_raw_fd(),
            Duration::from_millis(300),
        )
        .expect_err("a silent session should time out");
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ),
            "unexpected error {err:?}"
        );
        let waited = started.elapsed();
        assert!(
            waited < Duration::from_secs(5),
            "waited {waited:?}: the caller's timeout was overridden again"
        );
    }
}
