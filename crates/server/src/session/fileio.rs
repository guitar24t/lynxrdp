//! Bounded, nonblocking file adapters. Only the worker touches the filesystem.
//! Reads and final publication return WouldBlock until ready; the transfer
//! manager retries on FileReady. Dropping an adapter cancels its queued work.
use super::CoreEvent;
use crossbeam_channel::{bounded, Receiver, Sender};
use lynxrdp_proto::atomic_file::AtomicFile;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

const JOB_QUEUE: usize = 128;
const READ_CHUNK: usize = 64 * 1024;
const READ_AHEAD: usize = 4;
const WORKER_TIMEOUT: Duration = Duration::from_secs(120);

enum Job {
    Open {
        handle: u64,
        life: Weak<()>,
        path: String,
        id: u64,
        generation: u64,
        events: Sender<CoreEvent>,
    },
    Create {
        handle: u64,
        life: Weak<()>,
        root: PathBuf,
        relative: String,
        replace: bool,
        error: Arc<OnceLock<String>>,
    },
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
        reply: Sender<io::Result<()>>,
        error: Arc<OnceLock<String>>,
    },
}

/// An asynchronous open result, scoped to the connection that requested it.
#[derive(Debug)]
pub struct FileOpened {
    pub handle: u64,
    pub id: u64,
    pub generation: u64,
    pub path: String,
    pub result: Result<u64, String>,
}

#[derive(Clone, Debug)]
pub struct FileIo {
    jobs: Sender<Job>,
    handles: Arc<AtomicU64>,
}
impl FileIo {
    pub fn spawn(events: Sender<CoreEvent>) -> io::Result<Self> {
        let (jobs, rx) = bounded(JOB_QUEUE);
        std::thread::Builder::new()
            .name("file-io".into())
            .spawn(move || worker(rx, events))?;
        Ok(Self {
            jobs,
            handles: Arc::new(AtomicU64::new(1)),
        })
    }
    pub fn create(&self, root: PathBuf, relative: String, replace: bool) -> FileWriter {
        let handle = self.handles.fetch_add(1, Ordering::Relaxed);
        let life = Arc::new(());
        let error = Arc::new(OnceLock::new());
        if self
            .jobs
            .try_send(Job::Create {
                handle,
                life: Arc::downgrade(&life),
                root,
                relative,
                replace,
                error: error.clone(),
            })
            .is_err()
        {
            let _ = error.set("file worker is busy or stopped".into());
        }
        FileWriter {
            jobs: self.jobs.clone(),
            handle,
            _life: life,
            error,
            finishing: None,
            finished: false,
        }
    }
    pub fn open(
        &self,
        id: u64,
        generation: u64,
        path: String,
        events: Sender<CoreEvent>,
    ) -> FileReader {
        let handle = self.handles.fetch_add(1, Ordering::Relaxed);
        let life = Arc::new(());
        if self
            .jobs
            .try_send(Job::Open {
                handle,
                life: Arc::downgrade(&life),
                path: path.clone(),
                id,
                generation,
                events: events.clone(),
            })
            .is_err()
        {
            let _ = events.send(CoreEvent::FileOpened(Box::new(FileOpened {
                handle,
                id,
                generation,
                path,
                result: Err("file worker is busy or stopped".into()),
            })));
        }
        let (reply, answers) = bounded(READ_AHEAD);
        FileReader {
            jobs: self.jobs.clone(),
            handle,
            _life: life,
            answers,
            reply,
            outstanding: 0,
            chunk: Vec::new(),
            used: 0,
            eof: false,
            waiting: None,
            requested_at: Instant::now(),
        }
    }
}

pub struct FileWriter {
    jobs: Sender<Job>,
    handle: u64,
    _life: Arc<()>,
    error: Arc<OnceLock<String>>,
    finishing: Option<(Instant, Receiver<io::Result<()>>)>,
    finished: bool,
}
impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(e) = self.error.get() {
            return Err(io::Error::other(e.clone()));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        self.jobs
            .try_send(Job::Write {
                handle: self.handle,
                data: buf.to_vec(),
                error: self.error.clone(),
            })
            .map_err(|_| io::Error::other("file worker queue is full or stopped"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        if let Some(e) = self.error.get() {
            return Err(io::Error::other(e.clone()));
        }
        if self.finishing.is_none() {
            let (reply, answers) = bounded(1);
            self.jobs
                .try_send(Job::Flush {
                    handle: self.handle,
                    reply,
                    error: self.error.clone(),
                })
                .map_err(|_| io::Error::other("file worker queue is full or stopped"))?;
            self.finishing = Some((Instant::now(), answers));
        }
        let (since, answers) = self.finishing.as_ref().unwrap();
        match answers.try_recv() {
            Ok(result) => {
                result?;
                self.finished = true;
                Ok(())
            }
            Err(_) if since.elapsed() >= WORKER_TIMEOUT => Err(stalled()),
            Err(_) => Err(pending()),
        }
    }
}

pub struct FileReader {
    jobs: Sender<Job>,
    handle: u64,
    _life: Arc<()>,
    answers: Receiver<io::Result<Vec<u8>>>,
    reply: Sender<io::Result<Vec<u8>>>,
    outstanding: usize,
    chunk: Vec<u8>,
    used: usize,
    eof: bool,
    waiting: Option<Instant>,
    requested_at: Instant,
}
impl FileReader {
    pub fn handle(&self) -> u64 {
        self.handle
    }
    pub fn open_expired(&self) -> bool {
        self.requested_at.elapsed() >= WORKER_TIMEOUT
    }
    fn prefetch(&mut self) {
        while self.outstanding < READ_AHEAD {
            if self
                .jobs
                .try_send(Job::Read {
                    handle: self.handle,
                    reply: self.reply.clone(),
                })
                .is_err()
            {
                break;
            }
            self.outstanding += 1;
        }
    }
}
impl Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.eof {
            return Ok(0);
        }
        self.prefetch();
        if self.used == self.chunk.len() {
            match self.answers.try_recv() {
                Ok(result) => {
                    self.outstanding -= 1;
                    self.chunk = result?;
                    self.used = 0;
                    self.waiting = None;
                    if self.chunk.is_empty() {
                        self.eof = true;
                        return Ok(0);
                    }
                }
                Err(_) => {
                    if self.waiting.get_or_insert_with(Instant::now).elapsed() >= WORKER_TIMEOUT {
                        return Err(stalled());
                    }
                    return Err(pending());
                }
            }
        }
        let n = buf.len().min(self.chunk.len() - self.used);
        buf[..n].copy_from_slice(&self.chunk[self.used..self.used + n]);
        self.used += n;
        self.prefetch();
        Ok(n)
    }
}
fn pending() -> io::Error {
    io::Error::from(io::ErrorKind::WouldBlock)
}
fn stalled() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "file worker did not answer")
}

// Every component is opened relative to the preceding directory descriptor.
// No pathname check followed by a separate pathname open: a symlink swap in
// between cannot redirect the write. The final rename replaces a directory
// entry and never follows a symlink at the destination either.
fn confined_parent(root: &Path, relative: &str) -> io::Result<(File, String)> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let relative = lynxrdp_proto::transfer::safe_relative_path(relative)
        .ok_or_else(|| io::Error::other("unsafe destination"))?;
    std::fs::create_dir_all(root)?;
    let mut dir = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)?;
    let mut parts: Vec<_> = relative.split('/').collect();
    let name = parts.pop().unwrap().to_string();
    for part in parts {
        let part = CString::new(part)?;
        // SAFETY: dir is an owned open descriptor and part is NUL terminated.
        let fd = unsafe {
            if libc::mkdirat(dir.as_raw_fd(), part.as_ptr(), 0o700) < 0
                && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
            {
                return Err(io::Error::last_os_error());
            }
            libc::openat(
                dir.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh descriptor owned by this scope.
        dir = unsafe { File::from_raw_fd(fd) };
    }
    Ok((dir, name))
}
struct Output {
    file: AtomicFile,
    _parent: File,
}
impl Output {
    fn new(root: &Path, relative: &str, replace: bool) -> io::Result<Self> {
        use std::os::fd::AsRawFd;
        let (parent, name) = confined_parent(root, relative)?;
        let path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd())).join(name);
        Ok(Self {
            file: AtomicFile::new(&path, replace)?,
            _parent: parent,
        })
    }
}
enum Handle {
    Input(File),
    Output(Output),
}
fn worker(jobs: Receiver<Job>, events: Sender<CoreEvent>) {
    let mut open: HashMap<u64, (Weak<()>, Handle)> = HashMap::new();
    loop {
        open.retain(|_, (life, _)| life.strong_count() != 0);
        let job = match jobs.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => job,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(_) => return,
        };
        match job {
            Job::Open {
                handle,
                life,
                path,
                id,
                generation,
                events,
            } => {
                if life.strong_count() == 0 {
                    continue;
                }
                let result = File::open(&path)
                    .and_then(|file| {
                        let meta = file.metadata()?;
                        if !meta.is_file() {
                            return Err(io::Error::other("not a regular file"));
                        }
                        open.insert(handle, (life, Handle::Input(file)));
                        Ok(meta.len())
                    })
                    .map_err(|e| e.to_string());
                let _ = events.send(CoreEvent::FileOpened(Box::new(FileOpened {
                    handle,
                    id,
                    generation,
                    path,
                    result,
                })));
            }
            Job::Create {
                handle,
                life,
                root,
                relative,
                replace,
                error,
            } => {
                if life.strong_count() == 0 {
                    continue;
                }
                match Output::new(&root, &relative, replace) {
                    Ok(file) => {
                        open.insert(handle, (life, Handle::Output(file)));
                    }
                    Err(e) => {
                        let _ = error.set(e.to_string());
                    }
                }
            }
            Job::Read { handle, reply } => {
                let result = match open.get_mut(&handle) {
                    Some((_, Handle::Input(file))) => {
                        let mut data = vec![0; READ_CHUNK];
                        file.read(&mut data).map(|n| {
                            data.truncate(n);
                            data
                        })
                    }
                    _ => Err(io::Error::other("file closed")),
                };
                let _ = reply.try_send(result);
            }
            Job::Write {
                handle,
                data,
                error,
            } => {
                if error.get().is_some() {
                    continue;
                }
                let result = match open.get_mut(&handle) {
                    Some((_, Handle::Output(out))) => out.file.write_all(&data),
                    _ => Err(io::Error::other("file closed")),
                };
                if let Err(e) = result {
                    let _ = error.set(e.to_string());
                    open.remove(&handle);
                }
            }
            Job::Flush {
                handle,
                reply,
                error,
            } => {
                let result = if let Some(e) = error.get() {
                    Err(io::Error::other(e.clone()))
                } else {
                    match open.get_mut(&handle) {
                        Some((life, Handle::Output(out))) if life.strong_count() != 0 => {
                            out.file.flush()
                        }
                        _ => Err(io::Error::other("file closed")),
                    }
                };
                let _ = reply.try_send(result);
            }
        }
        let _ = events.send(CoreEvent::FileReady);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn flush(writer: &mut FileWriter) -> io::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match writer.flush() {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                result => return result,
            }
        }
    }
    #[test]
    fn publication_waits_for_all_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _) = crossbeam_channel::unbounded();
        let io = FileIo::spawn(tx).unwrap();
        let mut w = io.create(dir.path().to_path_buf(), "nested/file".into(), false);
        w.write_all(b"complete").unwrap();
        flush(&mut w).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("nested/file")).unwrap(),
            b"complete"
        );
    }
    #[test]
    fn full_queue_and_drop_never_wait_for_the_worker() {
        let (jobs, _rx) = bounded(1);
        let io = FileIo {
            jobs,
            handles: Arc::new(AtomicU64::new(1)),
        };
        let start = Instant::now();
        let mut a = io.create(PathBuf::from("/unused"), "a".into(), false);
        assert!(a.write_all(b"data").is_err());
        assert!(a.flush().is_err());
        let mut b = io.create(PathBuf::from("/unused"), "b".into(), false);
        assert!(b.flush().is_err());
        drop((a, b));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn symlink_parent_is_refused_and_final_symlink_is_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("precious");
        std::fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(Output::new(root.path(), "escape/precious", true).is_err());
        std::os::unix::fs::symlink(&target, root.path().join("link")).unwrap();
        let mut output = Output::new(root.path(), "link", true).unwrap();
        output.file.write_all(b"new").unwrap();
        output.file.flush().unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"original");
        assert_eq!(std::fs::read(root.path().join("link")).unwrap(), b"new");
    }
}
