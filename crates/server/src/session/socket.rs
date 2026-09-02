//! A cloneable, shutdown-able wrapper over a connected socket fd.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;

/// A connected stream socket (TCP or Unix) shared between reader and writer
/// threads. Dropping the last clone closes it.
#[derive(Clone)]
pub struct ClientSocket {
    fd: Arc<OwnedFd>,
}

impl ClientSocket {
    /// Wrap an owned socket fd.
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd: Arc::new(fd) }
    }

    /// Wrap a TCP stream.
    pub fn from_tcp(stream: TcpStream) -> Self {
        Self::from_fd(OwnedFd::from(stream))
    }

    /// Raw fd.
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Enable `TCP_NODELAY` (ignored on non-TCP sockets).
    pub fn set_nodelay(&self) {
        let one: libc::c_int = 1;
        // SAFETY: setsockopt with a valid int option value.
        unsafe {
            libc::setsockopt(
                self.raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_NODELAY,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    /// Enable keepalive probes so dead peers are noticed.
    pub fn set_keepalive(&self) {
        let one: libc::c_int = 1;
        // SAFETY: as above.
        unsafe {
            libc::setsockopt(
                self.raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_KEEPALIVE,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    /// Shut down both directions, waking any blocked reader.
    pub fn shutdown(&self) {
        // SAFETY: shutdown on a socket fd we own.
        unsafe {
            libc::shutdown(self.raw_fd(), libc::SHUT_RDWR);
        }
    }

    /// Local/peer description for logging, if it is an IP socket.
    pub fn describe(&self) -> String {
        // SAFETY: getpeername with a sockaddr_storage buffer of the right size.
        unsafe {
            let mut addr: libc::sockaddr_storage = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            if libc::getpeername(
                self.raw_fd(),
                &mut addr as *mut _ as *mut libc::sockaddr,
                &mut len,
            ) != 0
            {
                return "unknown".into();
            }
            match addr.ss_family as libc::c_int {
                libc::AF_INET => {
                    let a = &*(&addr as *const _ as *const libc::sockaddr_in);
                    let ip = std::net::Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
                    format!("{ip}:{}", u16::from_be(a.sin_port))
                }
                libc::AF_INET6 => {
                    let a = &*(&addr as *const _ as *const libc::sockaddr_in6);
                    let ip = std::net::Ipv6Addr::from(a.sin6_addr.s6_addr);
                    format!("[{ip}]:{}", u16::from_be(a.sin6_port))
                }
                libc::AF_UNIX => "unix".into(),
                other => format!("af{other}"),
            }
        }
    }
}

impl Read for ClientSocket {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: buf is valid for writes of buf.len() bytes.
            let n = unsafe {
                libc::read(
                    self.raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(n as usize);
        }
    }
}

impl Write for ClientSocket {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            // SAFETY: buf is valid for reads of buf.len() bytes.
            let n = unsafe {
                libc::send(
                    self.raw_fd(),
                    buf.as_ptr() as *const libc::c_void,
                    buf.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                // send() fails with ENOTSOCK on pipes (tests); fall back to write().
                if e.raw_os_error() == Some(libc::ENOTSOCK) {
                    let n = unsafe {
                        libc::write(
                            self.raw_fd(),
                            buf.as_ptr() as *const libc::c_void,
                            buf.len(),
                        )
                    };
                    if n < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    return Ok(n as usize);
                }
                return Err(e);
            }
            return Ok(n as usize);
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn read_write_and_shutdown() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        let sock = ClientSocket::from_tcp(s);
        sock.set_nodelay();
        sock.set_keepalive();
        assert!(sock.describe().starts_with("127.0.0.1:"));
        let mut w = sock.clone();
        w.write_all(b"abc").unwrap();
        let mut got = [0u8; 3];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"abc");
        let mut r = sock.clone();
        let t = std::thread::spawn(move || {
            let mut b = [0u8; 8];
            r.read(&mut b).unwrap()
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        sock.shutdown();
        assert_eq!(t.join().unwrap(), 0);
    }
}
