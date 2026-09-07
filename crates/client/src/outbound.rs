//! Ordered, bounded socket writes that never wait for the network on the UI
//! thread. An overloaded link is closed rather than dropping input (especially
//! key/button releases) or accumulating an unbounded backlog of stale input.
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{Receiver, Sender};

const MAX_MESSAGES: usize = 256;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const CLOSE_GRACE: Duration = Duration::from_millis(100);

pub(crate) struct Outbound {
    socket: TcpStream,
    sender: Option<Sender<Vec<u8>>>,
    queued_bytes: Arc<AtomicUsize>,
    error: Arc<Mutex<Option<String>>>,
    done: Receiver<()>,
    worker: Option<JoinHandle<()>>,
}

impl Outbound {
    pub fn new(socket: TcpStream) -> Result<Self> {
        let mut stream = socket.try_clone()?;
        let (sender, receiver) = crossbeam_channel::bounded::<Vec<u8>>(MAX_MESSAGES);
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let count = queued_bytes.clone();
        let error = Arc::new(Mutex::new(None));
        let failure = error.clone();
        let (finished, done) = crossbeam_channel::bounded(1);
        let worker = std::thread::Builder::new()
            .name("lynxrdp-writer".into())
            .spawn(move || {
                while let Ok(bytes) = receiver.recv() {
                    let result = stream.write_all(&bytes);
                    count.fetch_sub(bytes.len(), Ordering::Relaxed);
                    if let Err(e) = result {
                        failure
                            .lock()
                            .unwrap()
                            .get_or_insert_with(|| format!("sending message: {e}"));
                        // Wake the reader and hence the event loop, even if the
                        // peer continues sending while no longer accepting data.
                        let _ = stream.shutdown(Shutdown::Both);
                        break;
                    }
                }
                let _ = finished.send(());
            })
            .context("spawn writer")?;
        Ok(Self {
            socket,
            sender: Some(sender),
            queued_bytes,
            error,
            done,
            worker: Some(worker),
        })
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap().clone()
    }

    pub fn send(&self, bytes: Vec<u8>) -> Result<()> {
        if let Some(reason) = self.error() {
            return Err(anyhow!(reason));
        }
        let sender = self.sender.as_ref().context("connection writer closed")?;
        let size = bytes.len();
        if self
            .queued_bytes
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                n.checked_add(size).filter(|total| *total <= MAX_BYTES)
            })
            .is_err()
        {
            return self.fail("outgoing connection stalled: byte queue limit reached");
        }
        if sender.try_send(bytes).is_err() {
            self.queued_bytes.fetch_sub(size, Ordering::Relaxed);
            return self.fail("outgoing connection stalled: message queue unavailable");
        }
        Ok(())
    }

    fn fail(&self, reason: &str) -> Result<()> {
        let reason = self
            .error
            .lock()
            .unwrap()
            .get_or_insert_with(|| reason.to_owned())
            .clone();
        let _ = self.socket.shutdown(Shutdown::Both);
        Err(anyhow!(reason))
    }

    pub fn close(&mut self, graceful: bool) {
        // Disconnect the channel: ordinary closure drains queued input and the
        // goodbye in order. A stalled link gets only a short grace period.
        self.sender.take();
        if graceful && self.worker.is_some() {
            let _ = self.done.recv_timeout(CLOSE_GRACE);
        }
        // No write mutex is held here: shutdown must interrupt a blocked write.
        let _ = self.socket.shutdown(Shutdown::Both);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Outbound {
    fn drop(&mut self) {
        self.close(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::time::Instant;

    fn pair() -> (Outbound, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (peer, _) = listener.accept().unwrap();
        (Outbound::new(stream).unwrap(), peer)
    }

    #[test]
    fn graceful_close_preserves_message_order_and_collects_worker() {
        let (mut writer, mut peer) = pair();
        let reader = std::thread::spawn(move || {
            let mut received = Vec::new();
            peer.read_to_end(&mut received).unwrap();
            received
        });
        for byte in 0..100u8 {
            writer.send(vec![byte; 128]).unwrap();
        }
        writer.close(true);
        assert!(writer.worker.is_none());
        assert_eq!(
            reader.join().unwrap(),
            (0..100u8).flat_map(|b| vec![b; 128]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_stalled_peer_cannot_block_sends_or_abandonment_or_grow_the_queue() {
        let (mut writer, _unread_peer) = pair();
        let start = Instant::now();
        let mut overloaded = false;
        for _ in 0..4096 {
            if writer.send(vec![0; 64 * 1024]).is_err() {
                overloaded = true;
                break;
            }
            assert!(writer.queued_bytes.load(Ordering::Relaxed) <= MAX_BYTES);
        }
        assert!(
            overloaded,
            "a peer that never reads must hit the queue limit"
        );
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(writer.error().is_some());
        let start = Instant::now();
        writer.close(false);
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(writer.worker.is_none());
    }
}
