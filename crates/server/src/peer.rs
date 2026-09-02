//! Identification of the local user behind a connection.
//!
//! LynxRDP never asks for a password. Authentication is delegated entirely
//! to SSH: the client opens `ssh -L <port>:127.0.0.1:3390 user@host`, and
//! sshd's post-authentication process (which runs as `user`) is what
//! connects to us. We therefore only need to learn which local uid owns the
//! socket that connected to us:
//!
//! * for Unix sockets the kernel tells us directly (`SO_PEERCRED`);
//! * for loopback TCP we look the peer's socket up in `/proc/net/tcp{,6}`,
//!   exactly like an ident daemon does. Only root or the socket owner can
//!   see enough to spoof this, and both are already able to act as that
//!   user.
//!
//! Sessions are then created for exactly that uid.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

/// Identity of a connected peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerIdentity {
    /// Owner uid of the peer socket.
    pub uid: u32,
    /// Owner gid, when known (Unix sockets only).
    pub gid: Option<u32>,
    /// Process id of the peer, when known (Unix sockets only).
    pub pid: Option<i32>,
}

/// Identify the owner of the far end of a Unix stream socket.
pub fn unix_peer(stream: &UnixStream) -> io::Result<PeerIdentity> {
    // SAFETY: getsockopt with a correctly sized ucred buffer.
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        );
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PeerIdentity { uid: cred.uid, gid: Some(cred.gid), pid: Some(cred.pid) })
    }
}

/// Identify the owner of a loopback TCP connection.
///
/// Returns `Ok(None)` if the connection could not be found in the kernel
/// tables (already closed, or not a loopback connection).
pub fn tcp_peer(stream: &TcpStream) -> io::Result<Option<PeerIdentity>> {
    let local = stream.local_addr()?;
    let peer = stream.peer_addr()?;
    if !peer.ip().is_loopback() || !local.ip().is_loopback() {
        return Ok(None);
    }
    let uid = if peer.is_ipv4() {
        let text = std::fs::read_to_string("/proc/net/tcp")?;
        find_socket_owner(&text, &peer, &local)
    } else {
        let text = std::fs::read_to_string("/proc/net/tcp6")?;
        find_socket_owner(&text, &peer, &local)
    };
    Ok(uid.map(|uid| PeerIdentity { uid, gid: None, pid: None }))
}

/// Find the owner uid of the socket whose *local* address is `sock_local`
/// and whose *remote* address is `sock_remote`, in `/proc/net/tcp` text.
///
/// From our point of view the peer's socket has local = their address and
/// remote = our listening address. Only `ESTABLISHED` entries are matched.
pub fn find_socket_owner(
    proc_net_tcp: &str,
    sock_local: &SocketAddr,
    sock_remote: &SocketAddr,
) -> Option<u32> {
    for line in proc_net_tcp.lines().skip(1) {
        let mut f = line.split_whitespace();
        let _sl = f.next()?;
        let local = f.next()?;
        let remote = f.next()?;
        let state = f.next()?;
        let _txrx = f.next()?;
        let _trtm = f.next()?;
        let _retr = f.next()?;
        let uid = f.next()?;
        if state != "01" {
            continue;
        }
        let (Some(l), Some(r)) = (parse_proc_addr(local), parse_proc_addr(remote)) else {
            continue;
        };
        if l == *sock_local && r == *sock_remote {
            return uid.parse().ok();
        }
    }
    None
}

/// Parse a `/proc/net/tcp` address field (`HEXIP:HEXPORT`).
///
/// IPv4 addresses are one 32-bit word in host byte order (little endian on
/// every Linux target we support), IPv6 addresses are four such words.
pub fn parse_proc_addr(field: &str) -> Option<SocketAddr> {
    let (ip_hex, port_hex) = field.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    match ip_hex.len() {
        8 => {
            let word = u32::from_str_radix(ip_hex, 16).ok()?;
            let ip = Ipv4Addr::from(u32::from_be(word.to_le()).to_be_bytes());
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        32 => {
            let mut bytes = [0u8; 16];
            for (i, chunk) in ip_hex.as_bytes().chunks(8).enumerate() {
                let s = std::str::from_utf8(chunk).ok()?;
                let word = u32::from_str_radix(s, 16).ok()?;
                // Each 32-bit word is stored in host (little endian) order.
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(bytes)), port))
        }
        _ => None,
    }
}

/// The uid of the current process.
pub fn own_uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn parse_ipv4_field() {
        assert_eq!(
            parse_proc_addr("0100007F:0D3E"),
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3390))
        );
        assert_eq!(
            parse_proc_addr("0F02000A:0016"),
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 15)), 22))
        );
        assert_eq!(parse_proc_addr("bogus"), None);
        assert_eq!(parse_proc_addr("0100007F"), None);
    }

    #[test]
    fn parse_ipv6_field() {
        assert_eq!(
            parse_proc_addr("00000000000000000000000001000000:0D3E"),
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 3390))
        );
        // ::ffff:127.0.0.1
        assert_eq!(
            parse_proc_addr("0000000000000000FFFF00000100007F:0050"),
            Some(SocketAddr::new(
                IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()),
                80
            ))
        );
    }

    const SAMPLE: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:0D3E 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0
   1: 0100007F:A3F2 0100007F:0D3E 01 00000000:00000000 00:00000000 00000000  1000        0 22222 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:0D3E 0100007F:A3F2 01 00000000:00000000 00:00000000 00000000     0        0 22223 1 0000000000000000 20 4 30 10 -1
   3: 0100007F:A3F3 0100007F:0D3E 06 00000000:00000000 03:00000A0B 00000000  1001        0 0 3 0000000000000000
";

    #[test]
    fn finds_established_peer_socket() {
        let server = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3390);
        let client = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0xA3F2);
        assert_eq!(find_socket_owner(SAMPLE, &client, &server), Some(1000));
        // TIME_WAIT entry (state 06) must not match.
        let stale = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0xA3F3);
        assert_eq!(find_socket_owner(SAMPLE, &stale, &server), None);
        // Listening socket is not a peer.
        assert_eq!(find_socket_owner(SAMPLE, &server, &client), Some(0));
        assert_eq!(find_socket_owner("", &client, &server), None);
        assert_eq!(find_socket_owner("garbage\nmore garbage", &client, &server), None);
    }

    #[test]
    fn identifies_real_loopback_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        server.read_exact(&mut b).unwrap();
        let id = tcp_peer(&server).unwrap().expect("peer found");
        assert_eq!(id.uid, own_uid());
    }

    #[test]
    fn identifies_ipv6_loopback_connection() {
        let Ok(listener) = TcpListener::bind("[::1]:0") else {
            return; // no IPv6 on this host
        };
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        server.read_exact(&mut b).unwrap();
        let id = tcp_peer(&server).unwrap().expect("peer found");
        assert_eq!(id.uid, own_uid());
    }

    #[test]
    fn unix_peer_credentials() {
        let (a, b) = UnixStream::pair().unwrap();
        let id = unix_peer(&a).unwrap();
        assert_eq!(id.uid, own_uid());
        assert_eq!(id.pid, Some(std::process::id() as i32));
        drop(b);
    }
}
