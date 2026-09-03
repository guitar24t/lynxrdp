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
