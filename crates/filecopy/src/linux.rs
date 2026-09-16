//! Read-only clipboard references. Metadata never downloads file contents.
//! A file read queues a normal protocol transfer; only the filesystem worker
//! waits for it, leaving the session's input and frame loop responsive.
use crate::{Fetch, FetchReply, FETCH_IDLE};
use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Select, Sender};
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use lynxrdp_proto::{
    clipboard_batch::{safe_file_name, unique_name},
    FileEntry,
};
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

pub struct Files {
    // Close queued fetch replies before unmounting. Otherwise a kernel read
    // retried during disconnect can wait for a core that is unmounting us.
    pub requests: Receiver<Fetch>,
    _mount: BackgroundSession,
    _directory: tempfile::TempDir,
    pub paths: Vec<PathBuf>,
}
struct Read {
    index: usize,
    offset: u64,
    size: u32,
    reply: ReplyData,
}

/// A fetch in flight for one offered file, and the reads waiting on it.
///
/// Kept per index so that every read of a file that is still arriving --
/// the kernel's read-ahead, a second process, a file manager retrying --
/// waits on the one transfer rather than asking the core for another copy
/// of it.
struct Pending {
    reply: Receiver<FetchReply>,
    /// Bytes reported so far, and when that number last grew.
    received: u64,
    advanced: Instant,
    reads: Vec<Read>,
}

/// One turn of the read worker's loop.
enum Turn {
    /// A read arrived, or (`None`) the mount is being dropped.
    Read(Option<Read>),
    /// Fetch `index` reported, or (`None`) the core dropped its end.
    Reply(usize, Option<FetchReply>),
    /// Nothing happened before the earliest fetch's idle deadline.
    Idle,
}

/// Answer a read from the fetched copy.
fn serve(read: Read, path: &Path) {
    let result = (|| -> std::io::Result<Vec<u8>> {
        let mut data = vec![0; read.size as usize];
        let n = File::open(path)?.read_at(&mut data, read.offset)?;
        data.truncate(n);
        Ok(data)
    })();
    match result {
        Ok(data) => read.reply.data(&data),
        Err(e) => read.reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

/// Serve the fetch queue: start a transfer for the first read of each file,
/// park every read on the transfer it needs, answer them all when it lands.
///
/// A fetch is given up on after [`FETCH_IDLE`] without new bytes, and not
/// before: the old fixed cap was a file size above which pasting failed on a
/// slow enough link. Giving up is dropping the reply channel, which is what
/// tells the core to cancel the transfer -- so the next read of that file,
/// which the file manager's retry will send, starts one transfer and not a
/// second alongside the first.
fn read_worker(
    reads: Receiver<Read>,
    fetches: Sender<Fetch>,
    wake: Option<Box<dyn Fn() + Send>>,
    offered: Vec<FileEntry>,
    cache: PathBuf,
) {
    let mut cached: Vec<Option<PathBuf>> = (0..offered.len()).map(|_| None).collect();
    let mut fetching: BTreeMap<usize, Pending> = BTreeMap::new();
    let fail = |pending: Pending| {
        for read in pending.reads {
            read.reply.error(libc::EIO);
        }
    };
    loop {
        let keys: Vec<usize> = fetching.keys().copied().collect();
        let turn = {
            let mut select = Select::new();
            select.recv(&reads);
            for index in &keys {
                select.recv(&fetching[index].reply);
            }
            let deadline = keys
                .iter()
                .map(|index| fetching[index].advanced + FETCH_IDLE)
                .min();
            let selected = match deadline {
                Some(deadline) => select.select_deadline(deadline),
                None => Ok(select.select()),
            };
            match selected {
                Err(_) => Turn::Idle,
                Ok(op) if op.index() == 0 => Turn::Read(op.recv(&reads).ok()),
                Ok(op) => {
                    let index = keys[op.index() - 1];
                    Turn::Reply(index, op.recv(&fetching[&index].reply).ok())
                }
            }
        };
        match turn {
            Turn::Read(None) => return,
            Turn::Read(Some(read)) => {
                let index = read.index;
                if let Some(path) = &cached[index] {
                    serve(read, path);
                } else if let Some(pending) = fetching.get_mut(&index) {
                    pending.reads.push(read);
                } else {
                    // Room for a few progress reports between two turns of
                    // this loop; the core drops rather than blocks on a full
                    // channel.
                    let (tx, rx) = bounded(4);
                    let sent = fetches.send(Fetch {
                        remote: offered[index].path.clone(),
                        destination: cache.join(index.to_string()),
                        result: tx,
                    });
                    if sent.is_err() {
                        read.reply.error(libc::EIO);
                        continue;
                    }
                    if let Some(wake) = &wake {
                        wake();
                    }
                    fetching.insert(
                        index,
                        Pending {
                            reply: rx,
                            received: 0,
                            advanced: Instant::now(),
                            reads: vec![read],
                        },
                    );
                }
            }
            Turn::Reply(index, Some(FetchReply::Progress(n))) => {
                let pending = fetching.get_mut(&index).expect("selected fetch");
                if n > pending.received {
                    pending.received = n;
                    pending.advanced = Instant::now();
                }
            }
            Turn::Reply(index, Some(FetchReply::Done(path))) => {
                let pending = fetching.remove(&index).expect("selected fetch");
                let size = std::fs::metadata(&path).map(|m| m.len());
                if size.is_ok_and(|size| size == offered[index].size) {
                    for read in pending.reads {
                        serve(read, &path);
                    }
                    cached[index] = Some(path);
                } else {
                    for read in pending.reads {
                        read.reply.error(libc::ESTALE);
                    }
                }
            }
            Turn::Reply(index, None | Some(FetchReply::Failed)) => {
                fail(fetching.remove(&index).expect("selected fetch"));
            }
            Turn::Idle => {
                let now = Instant::now();
                let stalled: Vec<usize> = fetching
                    .iter()
                    .filter(|(_, pending)| now >= pending.advanced + FETCH_IDLE)
                    .map(|(index, _)| *index)
                    .collect();
                for index in stalled {
                    fail(fetching.remove(&index).expect("stalled fetch"));
                }
            }
        }
    }
}
struct View {
    names: Vec<String>,
    sizes: Vec<u64>,
    jobs: Sender<Read>,
    created: SystemTime,
}
impl Files {
    pub fn new(parent: &Path, files: &[FileEntry]) -> Result<Self> {
        Self::build(parent, files, None)
    }

    /// [`Files::new`], with `wake` called on the read thread each time a
    /// [`Fetch`] has been queued on `requests`.
    ///
    /// `requests` is a plain channel, and the side that drains it may be a
    /// select loop that this channel is not part of: the session core wakes
    /// for its own events and a housekeeping tick, so without a nudge every
    /// file's first read waited out that tick before its request left, one
    /// file after another. The hook is how the queue reaches such a loop.
    pub fn with_wake(
        parent: &Path,
        files: &[FileEntry],
        wake: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        Self::build(parent, files, Some(Box::new(wake)))
    }

    fn build(
        parent: &Path,
        files: &[FileEntry],
        wake: Option<Box<dyn Fn() + Send>>,
    ) -> Result<Self> {
        anyhow::ensure!(
            !files.is_empty() && files.len() <= 4096,
            "Invalid clipboard file count"
        );
        let directory = tempfile::Builder::new()
            .prefix("paste-")
            .tempdir_in(parent)?;
        let mountpoint = directory.path().join("files");
        let cache = directory.path().join("cache");
        std::fs::create_dir(&mountpoint)?;
        std::fs::create_dir(&cache)?;
        let mut taken = HashSet::new();
        let names: Vec<_> = files
            .iter()
            .map(|file| {
                let name = lynxrdp_proto::urilist::base_name(Path::new(&file.path))
                    .unwrap_or_else(|| "file".into());
                unique_name(&mut taken, &safe_file_name(&name))
            })
            .collect();
        let paths = names.iter().map(|name| mountpoint.join(name)).collect();
        let (jobs, reads) = bounded::<Read>(32);
        let (fetches, requests) = bounded(8);
        let offered = files.to_vec();
        std::thread::Builder::new()
            .name("clipboard-file-reads".into())
            .spawn(move || read_worker(reads, fetches, wake, offered, cache))?;
        let view = View {
            names,
            sizes: files.iter().map(|f| f.size).collect(),
            jobs,
            created: SystemTime::now(),
        };
        let mount = fuser::spawn_mount2(
            view,
            &mountpoint,
            &[
                MountOption::RO,
                MountOption::NoExec,
                MountOption::NoSuid,
                MountOption::NoDev,
                MountOption::FSName("lynxrdp-clipboard".into()),
            ],
        )
        .context("File paste requires /dev/fuse and fusermount3 on the server")?;
        Ok(Self {
            _mount: mount,
            _directory: directory,
            paths,
            requests,
        })
    }
}
impl View {
    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let (kind, size, perm, nlink) = if ino == 1 {
            (FileType::Directory, 0, 0o500, 2)
        } else {
            (
                FileType::RegularFile,
                *self.sizes.get(ino.checked_sub(2)? as usize)?,
                0o600,
                1,
            )
        };
        Some(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: self.created,
            mtime: self.created,
            ctime: self.created,
            crtime: self.created,
            kind,
            perm,
            nlink,
            uid: unsafe { libc::geteuid() },
            gid: unsafe { libc::getegid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
}
impl Filesystem for View {
    fn lookup(&mut self, _: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let index = (parent == 1)
            .then(|| self.names.iter().position(|n| OsStr::new(n) == name))
            .flatten();
        match index.and_then(|i| self.attr(i as u64 + 2)) {
            Some(attr) => reply.entry(&Duration::from_secs(1), &attr, 0),
            None => reply.error(libc::ENOENT),
        }
    }
    fn getattr(&mut self, _: &Request<'_>, ino: u64, _: Option<u64>, reply: ReplyAttr) {
        match self.attr(ino) {
            Some(attr) => reply.attr(&Duration::from_secs(1), &attr),
            None => reply.error(libc::ENOENT),
        }
    }
    fn open(&mut self, _: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            reply.error(libc::EROFS);
        } else if ino < 2 || self.attr(ino).is_none() {
            reply.error(libc::ENOENT);
        } else {
            reply.opened(0, 0);
        }
    }
    fn read(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        _: u64,
        offset: i64,
        size: u32,
        _: i32,
        _: Option<u64>,
        reply: ReplyData,
    ) {
        if ino < 2 || self.attr(ino).is_none() || offset < 0 || size > 1024 * 1024 {
            reply.error(libc::EINVAL);
            return;
        }
        let job = Read {
            index: (ino - 2) as usize,
            offset: offset as u64,
            size,
            reply,
        };
        if let Err(e) = self.jobs.try_send(job) {
            e.into_inner().reply.error(libc::EAGAIN);
        }
    }
    fn flush(&mut self, _: &Request<'_>, _: u64, _: u64, _: u64, reply: fuser::ReplyEmpty) {
        reply.ok();
    }

    fn readdir(
        &mut self,
        _: &Request<'_>,
        ino: u64,
        _: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino != 1 || offset < 0 {
            reply.error(libc::ENOTDIR);
            return;
        }
        let entries = [
            (1, FileType::Directory, "."),
            (1, FileType::Directory, ".."),
        ];
        for (i, (ino, kind, name)) in entries
            .into_iter()
            .chain(
                self.names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| (i as u64 + 2, FileType::RegularFile, n.as_str())),
            )
            .enumerate()
            .skip(offset as usize)
        {
            if reply.add(ino, i as i64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}
