//! Finder reads file URLs through macOS's built-in WebDAV filesystem.
//! No Finder scripting, extra app windows, or privileged filesystem extension.
use crate::{
    dav::{Server, USER},
    source::Source,
    Fetch,
};
use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// How long a forced unmount may take before it is abandoned. It runs on
/// whichever thread drops the offer, often the GUI's, so a wedged
/// webdavfs_agent must not hold the window; a healthy one takes well under a
/// second.
const UNMOUNT_BOUND: Duration = Duration::from_secs(5);

pub struct Files {
    // Readers must fail before the mount and its cache are removed.
    pub requests: Receiver<Fetch>,
    _mount: Mount,
    _server: Server,
    pub paths: Vec<PathBuf>,
}
struct Mount {
    path: PathBuf,
}
impl Drop for Mount {
    fn drop(&mut self) {
        // Synchronous on purpose: a detached unmount thread died with the
        // process on quit and left the volume in the mount table with nothing
        // behind it. Files drops the server only after this returns, so the
        // agent can still finish whatever the unmount needs from it.
        unmount(&self.path);
    }
}

/// `umount -f`, bounded by [`UNMOUNT_BOUND`]. Returns whether it succeeded.
fn unmount(path: &Path) -> bool {
    let child = Command::new("/sbin/umount")
        .arg("-f")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            log::warn!("could not start the unmount of {}: {e}", path.display());
            return false;
        }
    };
    let deadline = Instant::now() + UNMOUNT_BOUND;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                log::warn!(
                    "the unmount of {} did not finish within {UNMOUNT_BOUND:?}",
                    path.display()
                );
                return false;
            }
            Err(e) => {
                log::warn!("the unmount of {}: {e}", path.display());
                return false;
            }
        }
    }
}

/// Unmounts every copied-files volume the mount table still lists below a
/// staging root another process left behind, so the root can be removed.
/// Returns false while one remains: removing a root through a dead mount
/// would hang on the first stat inside it, which is why the mount table
/// rather than the directory tree is what gets consulted.
pub(crate) fn unmount_below(root: &Path) -> bool {
    // The kernel records the mount point resolved, /private/var/... for a
    // /var/... temporary directory.
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    let Ok(output) = Command::new("/sbin/mount").stdin(Stdio::null()).output() else {
        return false;
    };
    let mut clear = true;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        // "<url> on <mount point> (webdav, ...)"; the URL never contains " on ".
        let Some((_, rest)) = line.split_once(" on ") else {
            continue;
        };
        let Some((point, kind)) = rest.rsplit_once(" (") else {
            continue;
        };
        if kind.starts_with("webdav") && Path::new(point).starts_with(&root) {
            clear &= unmount(Path::new(point));
        }
    }
    clear
}

/// The file `mount_webdav -a<fd>` reads its credentials from, laid out as
/// Apple's own NetFS plugin (webdavfs' WebDAV_Mount.c) writes it: five
/// big-endian length-prefixed items, user, password, proxy user, proxy
/// password and the SSL properties, the last empty here. The open-source
/// drop's reader stops after four, but the shipped agent wants the fifth and
/// silently drops all the credentials without it. It lseeks to the start
/// before reading, so a pipe will not do. The file is unlinked as soon as it
/// is open and reaches the agent only as its stdin, which mount_webdav's fork
/// and exec pass through unchanged and which the agent reads while parsing
/// its options, before it closes the descriptors it inherited or points
/// stdin at /dev/null. Nothing secret is on its command line, where every
/// local user could read it.
fn credentials(directory: &Path, password: &str) -> Result<File> {
    let path = directory.join("credentials");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path)?;
    std::fs::remove_file(&path)?;
    let mut data = Vec::new();
    for item in [USER.as_bytes(), password.as_bytes(), &[], &[], &[]] {
        data.extend_from_slice(&(item.len() as u32).to_be_bytes());
        data.extend_from_slice(item);
    }
    file.write_all(&data)?;
    Ok(file)
}

impl Files {
    pub fn new(parent: &Path, files: &[lynxrdp_proto::FileEntry]) -> Result<Self> {
        let (source, requests) = Source::new(parent, files)?;
        let path = source.directory.path().join("files");
        std::fs::create_dir(&path)?;
        let paths = source.names.iter().map(|name| path.join(name)).collect();
        let server = Server::start(source.clone())?;
        let mount = Mount { path };
        let credentials = credentials(source.directory.path(), &server.password)?;
        let mut child = Command::new("/sbin/mount_webdav")
            .args([
                "-S",
                "-a0",
                "-o",
                "rdonly,nobrowse,nodev,nosuid",
                "-v",
                "Copied files",
            ])
            .arg(&server.url)
            .arg(&mount.path)
            .stdin(credentials)
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
    fn mount_table() -> String {
        let output = Command::new("/sbin/mount").output().unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
    /// Status line and body of one request made the way a local user would
    /// make it, from the shell with what `mount` and `ps` showed them.
    fn curl(args: &[&str]) -> (String, String) {
        let output = Command::new("/usr/bin/curl")
            .args(["-s", "-w", "\n%{http_code}"])
            .args(args)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let (body, code) = text.rsplit_once('\n').unwrap();
        (code.to_string(), body.to_string())
    }

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
        fetch
            .result
            .send(crate::FetchReply::Done(fetch.destination))
            .unwrap();
        assert_eq!(read.join().unwrap(), b"hello");
        assert!(files.requests.is_empty());
        // Dropping the offer is what unmounts, synchronously: the process may
        // be on its way out and nothing would wait for a thread.
        let url = files._server.url.clone();
        assert!(mount_table().contains(&url));
        drop(files);
        assert!(!mount_table().contains(&url), "{}", mount_table());
    }

    #[test]
    fn the_url_every_local_user_can_see_is_not_enough() {
        let parent = tempfile::tempdir().unwrap();
        let files = Files::new(
            parent.path(),
            &[lynxrdp_proto::FileEntry {
                path: "/remote/secret.txt".into(),
                size: 5,
            }],
        )
        .unwrap();
        let url = files._server.url.clone();
        let password = files._server.password.clone();
        // What another uid sees: the URL in the mount table and in the
        // agent's argv, and not the password in either.
        let table = mount_table();
        let point = std::fs::canonicalize(files.paths[0].parent().unwrap()).unwrap();
        assert!(
            table.lines().any(|line| line.contains(&url)
                && line.contains(&*point.to_string_lossy())
                && line.contains("webdav")),
            "{table}"
        );
        let processes = Command::new("/bin/ps")
            .args(["-axo", "args="])
            .output()
            .unwrap();
        let processes = String::from_utf8_lossy(&processes.stdout);
        assert!(
            processes
                .lines()
                .any(|line| line.contains("webdavfs_agent") && line.contains(&url)),
            "{processes}"
        );
        assert!(!processes.contains(&password));
        assert!(!table.contains(&password));
        // What they can do with it.
        let (code, body) = curl(&["-X", "PROPFIND", "-H", "Depth: 1", &url]);
        assert_eq!(code, "401", "{body}");
        assert!(!body.contains("secret.txt"));
        let (code, _) = curl(&[&format!("{url}secret.txt")]);
        assert_eq!(code, "401");
        assert!(
            files.requests.is_empty(),
            "a refused request must not fetch"
        );
        // What the agent, holding the credential, can do.
        let user = format!("{USER}:{password}");
        let (code, body) = curl(&["-u", &user, "-X", "PROPFIND", "-H", "Depth: 1", &url]);
        assert_eq!(code, "207", "{body}");
        assert!(body.contains("secret.txt"));
        assert!(files.requests.is_empty());
        drop(files);
        assert!(!mount_table().contains(&url));
    }
}
