//! Finder reads file URLs through macOS's built-in WebDAV filesystem.
//! No Finder scripting, extra app windows, or privileged filesystem extension.
use crate::{
    dav::Server,
    source::{Fetch, Source},
};
use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub struct Files {
    // Readers must fail before the mount and its cache are removed.
    pub requests: Receiver<Fetch>,
    _mount: Mount,
    _server: Server,
    pub paths: Vec<PathBuf>,
}
struct Mount {
    path: PathBuf,
    source: std::sync::Arc<Source>,
}
impl Drop for Mount {
    fn drop(&mut self) {
        // Detached cleanup keeps a busy native unmount off the GUI thread.
        let path = self.path.clone();
        let source = self.source.clone();
        let _ = std::thread::Builder::new()
            .name("clipboard-unmount".into())
            .spawn(move || {
                let _source = source;
                let _ = Command::new("/sbin/umount")
                    .arg("-f")
                    .arg(path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            });
    }
}
impl Files {
    pub fn new(parent: &Path, files: &[lynxrdp_proto::FileEntry]) -> Result<Self> {
        let (source, requests) = Source::new(parent, files)?;
        let path = source.directory.path().join("files");
        std::fs::create_dir(&path)?;
        let paths = source.names.iter().map(|name| path.join(name)).collect();
        let server = Server::start(source.clone())?;
        let mount = Mount { path, source };
        let mut child = Command::new("/sbin/mount_webdav")
            .args([
                "-S",
                "-o",
                "rdonly,nobrowse,nodev,nosuid",
                "-v",
                "Copied files",
            ])
            .arg(&server.url)
            .arg(&mount.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Starting macOS's deferred clipboard filesystem")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::ensure!(
                    status.success(),
                    "macOS could not mount copied file references ({status})"
                );
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("macOS clipboard filesystem setup timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(Self {
            requests,
            _mount: mount,
            _server: server,
            paths,
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn native_mount_reads_contents_only_after_metadata() {
        let parent = tempfile::tempdir().unwrap();
        let files = Files::new(
            parent.path(),
            &[lynxrdp_proto::FileEntry {
                path: "/remote/copied.txt".into(),
                size: 5,
            }],
        )
        .unwrap();
        assert_eq!(std::fs::metadata(&files.paths[0]).unwrap().len(), 5);
        assert!(files.requests.is_empty());
        let path = files.paths[0].clone();
        let destination = parent.path().join("pasted.txt");
        // Exercise macOS copyfile through std::fs::copy, as a file manager
        // does, rather than only testing a buffered Rust read of the source.
        let read = std::thread::spawn(move || {
            assert_eq!(std::fs::copy(path, &destination).unwrap(), 5);
            std::fs::read(destination).unwrap()
        });
        let fetch = files
            .requests
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        std::fs::write(&fetch.destination, b"hello").unwrap();
        fetch.result.send(Some(fetch.destination)).unwrap();
        assert_eq!(read.join().unwrap(), b"hello");
        assert!(files.requests.is_empty());
    }
}
