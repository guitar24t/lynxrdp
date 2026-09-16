//! File delivery for desktops without an icon/drop handler (notably GNOME Shell).
use anyhow::{bail, Context, Result};
use std::{
    collections::HashSet,
    ffi::{CString, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// How long `xdg-user-dir` may take to answer.
///
/// It reads one small file under the user's config directory, and the answer
/// when it cannot is the same `~/Desktop` it would have named anyway. What it
/// must not do is hold the delivery thread for as long as a home that has
/// stopped answering holds it, since the whole drop is waiting behind it.
const USER_DIR_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct Delivery {
    pub id: u64,
    pub result: crossbeam_channel::Receiver<Result<()>>,
    /// When the copy started, so the caller can give up on one that never
    /// finishes.
    pub started: Instant,
    cancelled: Arc<AtomicBool>,
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
pub(super) fn start(id: u64, roots: Vec<PathBuf>) -> Result<Delivery> {
    let (tx, result) = crossbeam_channel::bounded(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    std::thread::Builder::new()
        .name("desktop-file-drop".into())
        .spawn(move || {
            let outcome = desktop_directory().and_then(|dir| deliver(&roots, &dir, &flag));
            let _ = tx.send(outcome);
        })?;
    Ok(Delivery {
        id,
        result,
        started: Instant::now(),
        cancelled,
    })
}
fn desktop_directory() -> Result<PathBuf> {
    let home =
        PathBuf::from(std::env::var_os("HOME").context("No home directory for desktop files")?);
    let path = user_desktop_dir().unwrap_or_else(|| home.join("Desktop"));
    if !path.is_absolute() || path == home {
        bail!("The desktop folder is disabled in your desktop settings.");
    }
    std::fs::create_dir_all(&path)?;
    Ok(std::fs::canonicalize(path)?)
}
/// Ask `xdg-user-dir` where the Desktop folder is, for a bounded time.
///
/// `Command::output` would wait for as long as the helper takes, and a helper
/// stuck on a stalled home never answers. `None` for anything but a prompt,
/// successful answer, and the caller's `~/Desktop` fallback takes over.
fn user_desktop_dir() -> Option<PathBuf> {
    let mut child = Command::new("xdg-user-dir")
        .arg("DESKTOP")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + USER_DIR_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut bytes = child.wait_with_output().ok()?.stdout;
                while bytes.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    bytes.pop();
                }
                return Some(PathBuf::from(OsString::from_vec(bytes)));
            }
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
fn check(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Acquire) {
        bail!("Desktop drop cancelled");
    }
    Ok(())
}
fn copy_tree(source: &Path, destination: &Path, cancelled: &AtomicBool) -> Result<()> {
    check(cancelled)?;
    let meta = std::fs::symlink_metadata(source)?;
    if meta.is_dir() {
        std::fs::create_dir(destination)?;
        for item in std::fs::read_dir(source)? {
            let item = item?;
            copy_tree(&item.path(), &destination.join(item.file_name()), cancelled)?;
        }
    } else if meta.is_file() {
        std::fs::copy(source, destination)?;
    } else {
        bail!("Unsupported file in desktop drop");
    }
    Ok(())
}
fn deliver(roots: &[PathBuf], desktop: &Path, cancelled: &AtomicBool) -> Result<()> {
    let staging = tempfile::Builder::new()
        .prefix(".lynxrdp-drop-")
        .tempdir_in(desktop)?;
    // Finish copying the entire batch before publishing any selected root.
    for (i, source) in roots.iter().enumerate() {
        copy_tree(source, &staging.path().join(i.to_string()), cancelled)?;
    }
    for (i, source) in roots.iter().enumerate() {
        let name = source
            .file_name()
            .context("Missing desktop file name")?
            .to_string_lossy();
        let mut taken = HashSet::new();
        let staged = CString::new(staging.path().join(i.to_string()).as_os_str().as_bytes())?;
        loop {
            check(cancelled)?;
            let unique = lynxrdp_proto::clipboard_batch::unique_name(&mut taken, &name);
            let destination = CString::new(desktop.join(unique).as_os_str().as_bytes())?;
            // Both strings are NUL-terminated paths. NOREPLACE atomically
            // preserves existing files, folders and symlinks, even in a race.
            let status = unsafe {
                libc::renameat2(
                    libc::AT_FDCWD,
                    staged.as_ptr(),
                    libc::AT_FDCWD,
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if status == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn desktop_delivery_preserves_existing_files_and_nested_folders() {
        let sources = tempfile::tempdir().unwrap();
        let desktop = tempfile::tempdir().unwrap();
        std::fs::create_dir(sources.path().join("Folder")).unwrap();
        std::fs::write(sources.path().join("Folder/file.txt"), b"nested").unwrap();
        std::fs::write(sources.path().join("file.txt"), b"new").unwrap();
        std::fs::write(desktop.path().join("file.txt"), b"existing").unwrap();
        deliver(
            &[
                sources.path().join("file.txt"),
                sources.path().join("Folder"),
            ],
            desktop.path(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(desktop.path().join("file.txt")).unwrap(),
            b"existing"
        );
        assert_eq!(
            std::fs::read(desktop.path().join("file (2).txt")).unwrap(),
            b"new"
        );
        assert_eq!(
            std::fs::read(desktop.path().join("Folder/file.txt")).unwrap(),
            b"nested"
        );
    }
}
