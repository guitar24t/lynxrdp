//! Read-only clipboard references. Metadata never downloads file contents.
//! A file read queues a normal protocol transfer; only the filesystem worker
//! waits for it, leaving the session's input and frame loop responsive.
use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};
use lynxrdp_proto::{
    clipboard_batch::{safe_file_name, unique_name},
    FileEntry,
};
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs::File,
    os::unix::fs::FileExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub struct Fetch {
    pub remote: String,
    pub destination: PathBuf,
    pub result: Sender<Option<PathBuf>>,
}
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
struct View {
    names: Vec<String>,
    sizes: Vec<u64>,
    jobs: Sender<Read>,
    created: SystemTime,
}
impl Files {
    pub fn new(parent: &Path, files: &[FileEntry]) -> Result<Self> {
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
            .spawn(move || {
                let mut cached: Vec<Option<PathBuf>> = (0..offered.len()).map(|_| None).collect();
                while let Ok(read) = reads.recv() {
                    let result = (|| -> std::io::Result<Vec<u8>> {
                        if cached[read.index].is_none() {
                            let (tx, rx) = bounded(1);
                            fetches
                                .send(Fetch {
                                    remote: offered[read.index].path.clone(),
                                    destination: cache.join(read.index.to_string()),
                                    result: tx,
                                })
                                .map_err(|_| std::io::Error::from_raw_os_error(libc::EIO))?;
                            let path = rx
                                .recv_timeout(Duration::from_secs(300))
                                .ok()
                                .flatten()
                                .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EIO))?;
                            let file = File::open(&path)?;
                            if file.metadata()?.len() != offered[read.index].size {
                                return Err(std::io::Error::from_raw_os_error(libc::ESTALE));
                            }
                            cached[read.index] = Some(path);
                        }
                        let mut data = vec![0; read.size as usize];
                        let n = File::open(cached[read.index].as_ref().unwrap())?
                            .read_at(&mut data, read.offset)?;
                        data.truncate(n);
                        Ok(data)
                    })();
                    match result {
                        Ok(data) => read.reply.data(&data),
                        Err(e) => read.reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
                    }
                }
            })?;
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
