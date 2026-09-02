//! Passing file descriptors over Unix sockets (`SCM_RIGHTS`).

use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// Send `payload` together with `fd` over `sock`.
pub fn send_with_fd(sock: &UnixStream, payload: &[u8], fd: RawFd) -> io::Result<()> {
    assert!(!payload.is_empty(), "must send at least one byte with an fd");
    // SAFETY: we build a valid msghdr with one iovec and one SCM_RIGHTS control
    // message; all buffers outlive the sendmsg call.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, fd);
        loop {
            let n = libc::sendmsg(sock.as_raw_fd(), &msg, libc::MSG_NOSIGNAL);
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n as usize != payload.len() {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "short sendmsg"));
            }
            return Ok(());
        }
    }
}

/// Receive a payload (into `buf`) and an optional file descriptor.
/// Returns the number of payload bytes and the fd if one was attached.
pub fn recv_with_fd(sock: &UnixStream, buf: &mut [u8]) -> io::Result<(usize, Option<OwnedFd>)> {
    // SAFETY: valid msghdr with one iovec and a control buffer large enough for
    // one fd; MSG_CMSG_CLOEXEC ensures received fds are close-on-exec.
    unsafe {
        let mut iov =
            libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = space as _;
        let n = loop {
            let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            break n as usize;
        };
        let mut fd = None;
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let raw = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const RawFd);
                if raw >= 0 {
                    // Only one fd is ever sent; close any extras defensively.
                    if fd.is_none() {
                        fd = Some(OwnedFd::from_raw_fd(raw));
                    } else {
                        libc::close(raw);
                    }
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            return Err(io::Error::new(io::ErrorKind::Other, "control data truncated"));
        }
        Ok((n, fd))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn passes_tcp_socket_between_ends() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let (server_side, _) = listener.accept().unwrap();

        let (a, b) = UnixStream::pair().unwrap();
        send_with_fd(&a, b"hello", server_side.as_raw_fd()).unwrap();
        drop(server_side);

        let mut buf = [0u8; 16];
        let (n, fd) = recv_with_fd(&b, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        let fd = fd.expect("fd received");
        let mut received = TcpStream::from(fd);

        client.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        received.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");
    }

    #[test]
    fn payload_without_fd() {
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(b"plain").unwrap();
        let mut buf = [0u8; 16];
        let (n, fd) = recv_with_fd(&b, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"plain");
        assert!(fd.is_none());
    }
}
