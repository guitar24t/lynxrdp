//! Accepting client connections in the session process.

use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::thread::JoinHandle;

use crossbeam_channel::Sender;

use super::socket::ClientSocket;
use super::{CoreEvent, NewClient};
use crate::handoff::{self, Reply};
use crate::peer;

/// Accept loopback TCP connections directly ("user mode").
///
/// Every connection's owner is identified through `/proc/net/tcp`; when
/// `require_uid` is set, connections from any other uid are dropped.
pub fn spawn_tcp_listener(
    listener: TcpListener,
    tx: Sender<CoreEvent>,
    require_uid: Option<u32>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("tcp-listener".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let stream = match conn {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("accept failed: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                };
                let desc = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "?".into());
                if let Some(required) = require_uid {
                    match peer::tcp_peer(&stream) {
                        Ok(Some(id)) if id.uid == required => {}
                        Ok(Some(id)) => {
                            log::warn!(
                                "refusing connection from {desc}: uid {} != {required}",
                                id.uid
                            );
                            continue;
                        }
                        Ok(None) => {
                            log::warn!(
                                "refusing connection from {desc}: peer could not be identified"
                            );
                            continue;
                        }
                        Err(e) => {
                            log::warn!("refusing connection from {desc}: {e}");
                            continue;
                        }
                    }
                }
                let socket = ClientSocket::from_tcp(stream);
                if tx
                    .send(CoreEvent::NewClient(NewClient {
                        socket,
                        description: desc,
                    }))
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("spawn listener thread")
}

/// Accept handoffs from `lynxrdpd` on the control socket.
pub fn spawn_control_listener(
    listener: UnixListener,
    tx: Sender<CoreEvent>,
    own_uid: u32,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("control-listener".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let stream = match conn {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("control accept failed: {e}");
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        continue;
                    }
                };
                match peer::unix_peer(&stream) {
                    Ok(id) if id.uid == 0 || id.uid == own_uid => {}
                    Ok(id) => {
                        log::warn!("ignoring control connection from uid {}", id.uid);
                        continue;
                    }
                    Err(e) => {
                        log::warn!("ignoring control connection: {e}");
                        continue;
                    }
                }
                let (h, fd) = match handoff::recv_handoff(&stream) {
                    Ok(x) => x,
                    Err(e) => {
                        log::warn!("bad handoff: {e}");
                        continue;
                    }
                };
                if h.uid != own_uid {
                    log::warn!("refusing handoff for uid {} (we are {own_uid})", h.uid);
                    let _ = handoff::send_reply(&stream, Reply::Refused);
                    continue;
                }
                let socket = ClientSocket::from_fd(fd);
                let description = format!("{} via lynxrdpd", h.peer);
                if handoff::send_reply(&stream, Reply::Accepted).is_err() {
                    continue;
                }
                if tx
                    .send(CoreEvent::NewClient(NewClient {
                        socket,
                        description,
                    }))
                    .is_err()
                {
                    break;
                }
            }
        })
        .expect("spawn control listener thread")
}

/// Forward messages from a client socket to the core until it closes.
pub fn spawn_client_reader(
    socket: ClientSocket,
    generation: u64,
    tx: Sender<CoreEvent>,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("client-{generation}-reader"))
        .spawn(move || {
            let mut reader = std::io::BufReader::with_capacity(64 * 1024, socket);
            loop {
                match lynxrdp_proto::frame::read_message(&mut reader) {
                    Ok(msg) => {
                        if tx.send(CoreEvent::ClientMessage(generation, msg)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let reason = if e.is_disconnect() {
                            "connection closed".to_string()
                        } else {
                            e.to_string()
                        };
                        let _ = tx.send(CoreEvent::ClientClosed(generation, reason));
                        break;
                    }
                }
            }
        })
        .expect("spawn reader thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    use crossbeam_channel::Receiver;

    /// A listener on a fresh loopback port and the channel it admits into.
    ///
    /// The thread stays blocked in `accept` for the rest of the test binary;
    /// there is nothing it holds that is worth the ceremony of reclaiming.
    fn listening(require_uid: Option<u32>) -> (SocketAddr, Receiver<CoreEvent>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        spawn_tcp_listener(listener, tx, require_uid);
        (addr, rx)
    }

    #[test]
    fn a_connection_from_the_required_uid_is_admitted() {
        let (addr, rx) = listening(Some(peer::own_uid()));
        let _client = TcpStream::connect(addr).unwrap();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(CoreEvent::NewClient(client)) => assert!(
                client.description.starts_with("127.0.0.1:"),
                "{}",
                client.description
            ),
            Ok(_) => panic!("the listener sent something other than a new client"),
            Err(e) => panic!("the listener did not admit its own user: {e}"),
        }
    }

    /// The peer is this process, so from the listener's side any uid but ours
    /// is another user's. This is the branch SECURITY.md relies on for
    /// `--listen` mode, and the end-to-end suite cannot reach it: it has no
    /// second uid to connect as. The test above is what makes this one
    /// meaningful -- it shows the `/proc/net/tcp` lookup identifies a peer
    /// here, so the refusal below is the mismatch and not a failed lookup.
    #[test]
    fn a_connection_from_another_uid_is_refused() {
        let (addr, rx) = listening(Some(peer::own_uid().wrapping_add(1)));
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // A refused connection is dropped without a word. Only the listener
        // holds the accepted end, so EOF here means it let go; an admitted
        // socket would be sitting open in the channel instead.
        let mut buf = [0u8; 1];
        assert_eq!(
            client.read(&mut buf).unwrap(),
            0,
            "the connection was not closed"
        );
        assert!(
            rx.try_recv().is_err(),
            "a refused connection reached the core"
        );
    }

    /// What `--insecure-skip-peer-check` passes.
    #[test]
    fn the_check_can_be_switched_off() {
        let (addr, rx) = listening(None);
        let _client = TcpStream::connect(addr).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(CoreEvent::NewClient(_))
        ));
    }
}
