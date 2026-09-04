//! Transfer file I/O, executed on a worker thread instead of the session core.
//!
//! `transfer.rs` documents two protections against a transfer starving the
//! screen, and both of them bound *bytes*: a chunk is at most `CHUNK_SIZE`, and
//! a sender keeps at most `WINDOW_CHUNKS` chunks unacknowledged. Neither bounds
//! *time*. Every one of those bytes still reached the disk through a `read` or
//! `write_all` on the thread that owns capture, encode and input injection, and
//! `create_dir_all`, `File::create` and `File::open` ran there too. On a home
//! directory over NFS -- the ordinary arrangement on the machines this server
//! is for -- a 50 ms server round trip per 64 KiB chunk is invisible in a file
//! manager and fatal in a frame loop: the whole desktop hitches for the length
//! of the transfer.
//!
//! So one worker thread per *session* owns every open file and the core talks
//! to it over a channel. Per session and not per transfer, because
//! `stage_client_files` fans out one transfer per clipboard file and is bounded
//! only by `MAX_FILE_LIST` (4096) -- a thread each would be a denial of service
//! a peer could ask for by copying a directory.
//!
//! The adapters here are all the rest of the code sees, and they fit the types
//! `crates/proto` already erases -- `Sink::Stream(Box<dyn Write + Send>)` and
//! `offer_stream_with_id(.., Box<dyn Read + Send>)` -- so nothing on the wire
//! and nothing in the protocol crate changes.
//!
//! Two places still park the core thread, both deliberately:
//!
//! * [`FileWriter::flush`] is a barrier. `TransferManager::complete_incoming`
//!   hands the staged paths to the X clipboard the moment `finish()` returns,
//!   and an X selection owner must answer a paste immediately -- it cannot go
//!   back and wait. A flush that returned before the bytes were written would
//!   therefore advertise half-written files to whatever the user pastes into.
//! * [`FileReader::read`] waits when its read-ahead is empty. `Read` has no way
//!   to say "not yet" that `TransferSender` would not read as end of file, and
//!   inventing one means changing the protocol crate. Instead the reader keeps
//!   [`READ_AHEAD`] chunks on order at all times, so after the opening chunk
//!   the worker is running ahead of the sender and the core only waits when the
//!   disk is genuinely slower than the link -- at which point something has to.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

use super::CoreEvent;

/// Jobs that may be queued before a submission blocks.
///
/// A write job carries at most one transfer chunk (64 KiB), so this is also
/// the memory the queue may hold: 8 MiB. Blocking past that is the point. It
/// is the only backpressure there is between a link faster than the disk and
/// a queue that would otherwise grow until the session is killed for it.
const JOB_QUEUE: usize = 128;

/// Bytes the worker reads per request.
const READ_CHUNK: usize = 64 * 1024;

/// Read requests one download may have outstanding at once.
///
/// Half the transfer's own acknowledgement window (`WINDOW_CHUNKS` is 8), which
/// is what the sender asks for in one burst the moment the peer accepts. Deeper
/// would buy nothing and cost 64 KiB a slot on every transfer at once, and a
/// clipboard copy can put a lot of transfers in flight.
const READ_AHEAD: usize = 4;

/// How long an adapter waits for the worker before giving up on it.
///
/// Read what this actually bounds, because it is not one operation. A `Flush`
/// or a `Read` is answered only when everything queued ahead of it has run, so
/// the wait is up to [`JOB_QUEUE`] filesystem operations -- 8 MiB of writes at
/// worst. Two minutes covers that on any mount that is still answering at all
/// (8 MiB in two minutes is 70 KiB a second) and does not cover a mount that
/// has stopped, which is the distinction the timeout is drawing.
///
/// It exists so a wedged worker degrades to a failed transfer rather than a
/// permanently frozen desktop: the core thread must never be parked on
/// something with no upper bound, which is the whole reason for this module.
/// Two minutes is still a long freeze -- it is a backstop, not a target, and
/// the read-ahead below is what keeps the ordinary case off it entirely.
const WORKER_TIMEOUT: Duration = Duration::from_secs(120);

/// Work for the file thread. Ordering matters and the channel preserves it:
/// a `Flush` for a handle is only reached once every `Write` queued for that
/// handle before it has been executed, which is what makes flush a barrier.
enum Job {
    /// Open a file for reading and report its size to the core loop.
    Open {
        handle: u64,
        path: String,
        id: u64,
        generation: u64,
        events: Sender<CoreEvent>,
    },
    /// Create a file, and any parent directories it needs, for writing.
    Create {
        handle: u64,
        path: PathBuf,
        error: Arc<OnceLock<String>>,
    },
    /// Read the next [`READ_CHUNK`] bytes; an empty answer means end of file.
    Read {
        handle: u64,
        reply: Sender<io::Result<Vec<u8>>>,
    },
    Write {
        handle: u64,
        data: Vec<u8>,
        error: Arc<OnceLock<String>>,
    },
    Flush {
        handle: u64,
        error: Arc<OnceLock<String>>,
        reply: Sender<Result<(), String>>,
    },
    Close {
        handle: u64,
    },
}

/// What the worker made of a client's `FileRequest`.
#[derive(Debug)]
pub struct FileOpened {
    /// Transfer identifier the client chose.
    pub id: u64,
    /// Client generation the request came from, so a request answered after
    /// that client went away is dropped instead of offered to its successor.
    pub generation: u64,
    /// The path exactly as the client named it.
    pub path: String,
    /// Size in bytes, or why the file could not be opened.
    pub result: Result<u64, String>,
}

/// A handle on the session's file worker. Cheap to clone; every clone talks to
/// the same thread.
#[derive(Clone)]
pub struct FileIo {
    jobs: Sender<Job>,
    handles: Arc<AtomicU64>,
}

impl FileIo {
    /// Start the worker.
    pub fn spawn() -> io::Result<Self> {
        let (jobs, rx) = bounded(JOB_QUEUE);
        // Detached on purpose. The thread ends by itself when the last sender
        // drops, and it drains what is queued on the way out; a join handle we
        // never join would only be an invitation to wait on a filesystem that
        // has stopped answering. Nothing that matters is lost either way,
        // because every completed transfer has already been through
        // `FileWriter::flush`, which is a barrier.
        std::thread::Builder::new()
            .name("file-io".into())
            .spawn(move || worker(&rx))?;
        Ok(Self {
            jobs,
            handles: Arc::new(AtomicU64::new(1)),
        })
    }

    /// A file the peer is sending us: an upload, or a clipboard file we asked
    /// for while staging a copy.
    ///
    /// The create is queued, not performed, so the caller is not told here
    /// whether the path can be written. That failure surfaces from
    /// [`FileWriter::flush`], which every incoming transfer runs through
    /// `TransferReceiver::finish`, so it still reaches the peer -- late rather
    /// than never, which is the price of not doing a `mkdir` on the frame
    /// thread.
    pub fn create(&self, path: PathBuf) -> FileWriter {
        let handle = self.next_handle();
        let error = Arc::new(OnceLock::new());
        let job = Job::Create {
            handle,
            path,
            error: error.clone(),
        };
        if self.jobs.send(job).is_err() {
            let _ = error.set("the session file worker has stopped".to_string());
        }
        FileWriter {
            jobs: self.jobs.clone(),
            handle,
            error,
        }
    }

    /// Open a file the client asked to download.
    ///
    /// The answer arrives on the core loop as [`CoreEvent::FileOpened`],
    /// because the offer needs the file's size and a `stat` on a hung mount is
    /// exactly what this module exists to keep off the core thread. The reader
    /// is returned straight away so the caller can hold it until then -- and
    /// drop it, which closes the file, if the open failed or the client left.
    pub fn open(
        &self,
        id: u64,
        generation: u64,
        path: String,
        events: Sender<CoreEvent>,
    ) -> FileReader {
        let handle = self.next_handle();
        let job = Job::Open {
            handle,
            path: path.clone(),
            id,
            generation,
            events: events.clone(),
        };
        if self.jobs.send(job).is_err() {
            // The worker is gone (the session is shutting down). Answer from
            // here so the request resolves instead of leaving the client
            // waiting for an offer that is never coming.
            let _ = events.send(CoreEvent::FileOpened(Box::new(FileOpened {
                id,
                generation,
                path,
                result: Err("the session file worker has stopped".to_string()),
            })));
        }
        FileReader::new(self.jobs.clone(), handle)
    }

    fn next_handle(&self) -> u64 {
        self.handles.fetch_add(1, Ordering::Relaxed)
    }
}

impl std::fmt::Debug for FileIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileIo({} queued)", self.jobs.len())
    }
}

/// A file the worker is writing, used as a transfer `Sink::Stream`.
pub struct FileWriter {
    jobs: Sender<Job>,
    handle: u64,
    /// The first failure on this file, recorded by the worker. `write` fails
    /// fast once it is set, so a doomed upload stops consuming bandwidth, and
    /// `flush` reports it, which is what fails the transfer.
    error: Arc<OnceLock<String>>,
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(e) = self.error.get() {
            return Err(io::Error::other(e.clone()));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let job = Job::Write {
            handle: self.handle,
            data: buf.to_vec(),
            error: self.error.clone(),
        };
        match self.jobs.try_send(job) {
            Ok(()) => Ok(buf.len()),
            Err(TrySendError::Full(job)) => {
                // Waiting here costs one filesystem operation, not the whole
                // queue: the worker frees a slot as soon as it finishes the
                // write it is on. That is the backpressure, and it only ever
                // engages once the peer is 8 MiB ahead of the disk.
                self.jobs.send(job).map_err(|_| worker_gone())?;
                Ok(buf.len())
            }
            Err(TrySendError::Disconnected(_)) => Err(worker_gone()),
        }
    }

    /// Wait until everything written so far has actually been written.
    ///
    /// Blocking is the contract, not an oversight -- see the module comment.
    /// There is no `fsync` here on purpose: what the clipboard needs is that
    /// another process on this host can read the whole file, not that it
    /// survives a power cut, and an `fsync` per staged file would put the very
    /// stall this module removes back on the core thread.
    fn flush(&mut self) -> io::Result<()> {
        let (reply, answers) = bounded(1);
        let job = Job::Flush {
            handle: self.handle,
            error: self.error.clone(),
            reply,
        };
        self.jobs.send(job).map_err(|_| worker_gone())?;
        match answers.recv_timeout(WORKER_TIMEOUT) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(io::Error::other(e)),
            Err(_) => Err(worker_stalled()),
        }
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        close(&self.jobs, self.handle);
    }
}

/// A file the worker is reading, used as a transfer source.
pub struct FileReader {
    jobs: Sender<Job>,
    handle: u64,
    answers: Receiver<io::Result<Vec<u8>>>,
    /// Kept so the worker can answer requests we have not issued yet; also why
    /// `answers` never reports itself disconnected, hence the timeout below.
    reply: Sender<io::Result<Vec<u8>>>,
    /// Read requests queued but not yet consumed.
    outstanding: usize,
    /// The chunk being handed out, and how much of it has gone.
    chunk: Vec<u8>,
    used: usize,
    eof: bool,
}

impl FileReader {
    fn new(jobs: Sender<Job>, handle: u64) -> Self {
        // Capacity equal to the read-ahead depth, so the worker can never block
        // delivering an answer: it has every other file in the session to serve.
        let (reply, answers) = bounded(READ_AHEAD);
        Self {
            jobs,
            handle,
            answers,
            reply,
            outstanding: 0,
            chunk: Vec::new(),
            used: 0,
            eof: false,
        }
    }

    /// Top the read-ahead up without ever waiting. A request that will not fit
    /// in the queue is simply not made; the next `read` will make it.
    fn prefetch(&mut self) {
        while self.outstanding < READ_AHEAD {
            let job = Job::Read {
                handle: self.handle,
                reply: self.reply.clone(),
            };
            if self.jobs.try_send(job).is_err() {
                break;
            }
            self.outstanding += 1;
        }
    }

    /// Guarantee at least one request is queued, so the receive that follows
    /// has an answer coming. This is the only submission here that may wait.
    fn ensure_queued(&mut self) -> io::Result<()> {
        if self.outstanding == 0 {
            let job = Job::Read {
                handle: self.handle,
                reply: self.reply.clone(),
            };
            self.jobs.send(job).map_err(|_| worker_gone())?;
            self.outstanding = 1;
        }
        self.prefetch();
        Ok(())
    }
}

impl Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.used >= self.chunk.len() {
            if self.eof {
                return Ok(0);
            }
            self.ensure_queued()?;
            let answer = self
                .answers
                .recv_timeout(WORKER_TIMEOUT)
                .map_err(|_| worker_stalled())?;
            self.outstanding -= 1;
            let data = answer?;
            if data.is_empty() {
                self.eof = true;
                return Ok(0);
            }
            self.chunk = data;
            self.used = 0;
        }
        let n = buf.len().min(self.chunk.len() - self.used);
        buf[..n].copy_from_slice(&self.chunk[self.used..self.used + n]);
        self.used += n;
        // Keep the worker a window ahead while the core gets on with a frame.
        self.prefetch();
        Ok(n)
    }
}

impl Drop for FileReader {
    fn drop(&mut self) {
        close(&self.jobs, self.handle);
    }
}

fn worker_gone() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "the session file worker has stopped",
    )
}

fn worker_stalled() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "the session file worker did not answer",
    )
}

/// Ask the worker to close a handle.
///
/// Worth waiting for a queue slot: a clipboard copy can stage up to
/// `MAX_FILE_LIST` files at once, and a descriptor held until the session ends
/// is a worse outcome than one filesystem operation of delay here.
fn close(jobs: &Sender<Job>, handle: u64) {
    if let Err(TrySendError::Full(job)) = jobs.try_send(Job::Close { handle }) {
        let _ = jobs.send(job);
    }
}

fn create_with_parents(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    File::create(path)
}

/// Read up to [`READ_CHUNK`] bytes, filling across short reads so an awkward
/// source does not fragment the transfer into tiny messages.
fn read_chunk(file: &mut File) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

fn worker(jobs: &Receiver<Job>) {
    let mut open: HashMap<u64, File> = HashMap::new();
    for job in jobs {
        match job {
            Job::Open {
                handle,
                path,
                id,
                generation,
                events,
            } => {
                let result = match File::open(&path) {
                    Ok(file) => match file.metadata() {
                        Ok(meta) => {
                            let size = meta.len();
                            open.insert(handle, file);
                            Ok(size)
                        }
                        Err(e) => Err(e.to_string()),
                    },
                    Err(e) => Err(e.to_string()),
                };
                let _ = events.send(CoreEvent::FileOpened(Box::new(FileOpened {
                    id,
                    generation,
                    path,
                    result,
                })));
            }
            Job::Create {
                handle,
                path,
                error,
            } => match create_with_parents(&path) {
                Ok(file) => {
                    open.insert(handle, file);
                }
                Err(e) => {
                    let _ = error.set(format!("cannot write {}: {e}", path.display()));
                }
            },
            Job::Read { handle, reply } => {
                let answer = match open.get_mut(&handle) {
                    Some(file) => read_chunk(file),
                    None => Err(io::Error::other("the file is no longer open")),
                };
                let _ = reply.send(answer);
            }
            Job::Write {
                handle,
                data,
                error,
            } => {
                // A file that has already failed swallows the rest of its
                // transfer without touching the disk again; the failure is
                // reported once, from the flush.
                if error.get().is_some() {
                    continue;
                }
                match open.get_mut(&handle) {
                    Some(file) => {
                        if let Err(e) = file.write_all(&data) {
                            let _ = error.set(e.to_string());
                            open.remove(&handle);
                        }
                    }
                    None => {
                        let _ = error.set("the file is not open".to_string());
                    }
                }
            }
            Job::Flush {
                handle,
                error,
                reply,
            } => {
                if let Some(file) = open.get_mut(&handle) {
                    if let Err(e) = file.flush() {
                        let _ = error.set(e.to_string());
                    }
                }
                let result = match error.get() {
                    Some(msg) => Err(msg.clone()),
                    None => Ok(()),
                };
                let _ = reply.send(result);
            }
            Job::Close { handle } => {
                open.remove(&handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write path is a queue, so the bytes are on disk when the barrier
    /// returns and not merely accepted. `complete_incoming` publishes staged
    /// paths to the X clipboard the instant this returns.
    #[test]
    fn flush_is_a_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep/nested/file.bin");
        let io = FileIo::spawn().unwrap();
        let mut w = io.create(path.clone());
        let payload = vec![7u8; 300_000];
        w.write_all(&payload).unwrap();
        w.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    /// A create that cannot succeed still has to reach the peer. It cannot be
    /// reported from `accept`, because that is the call being kept off the
    /// disk, so it comes back from the flush that ends every transfer.
    #[test]
    fn a_failed_create_is_reported_by_the_flush() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let io = FileIo::spawn().unwrap();
        let mut w = io.create(blocker.join("child.bin"));
        // The write may or may not be refused yet -- the create is racing it --
        // but the flush must be.
        let _ = w.write_all(b"payload");
        assert!(w.flush().is_err(), "a doomed create flushed cleanly");
    }

    #[test]
    fn a_reader_streams_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.bin");
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).unwrap();

        let io = FileIo::spawn().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut reader = io.open(9, 1, path.to_string_lossy().into_owned(), tx);
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CoreEvent::FileOpened(o) => {
                assert_eq!(o.id, 9);
                assert_eq!(o.generation, 1);
                assert_eq!(o.result.unwrap(), payload.len() as u64);
            }
            _ => panic!("the worker answered with something else"),
        }
        let mut got = Vec::new();
        reader.read_to_end(&mut got).unwrap();
        assert_eq!(got, payload);
    }

    /// A missing file must resolve, and must name itself when it does: the
    /// client turns this into the error the user sees.
    #[test]
    fn opening_a_missing_file_answers_with_a_reason() {
        let io = FileIo::spawn().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let _reader = io.open(3, 1, "/definitely/not/here.txt".to_string(), tx);
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            CoreEvent::FileOpened(o) => assert!(o.result.is_err(), "{:?}", o.result),
            _ => panic!("the worker answered with something else"),
        }
    }
}
