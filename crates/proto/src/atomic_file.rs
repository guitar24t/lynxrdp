//! Publish a received file only after the entire transfer has succeeded.
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Existing destinations are preserved unless replacement was explicitly chosen.
pub struct AtomicFile {
    staged: Option<tempfile::NamedTempFile>,
    destination: PathBuf,
    replace: bool,
}

impl AtomicFile {
    /// The caller is responsible for validating and creating the parent directory.
    pub fn new(destination: &Path, replace: bool) -> io::Result<Self> {
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut builder = tempfile::Builder::new();
        builder.prefix(".lynxrdp-transfer-");
        // `tempfile` creates its files 0600, which is right for a secret and
        // wrong for a document: the staging file *becomes* the published one
        // by rename, so every upload and download came out private whatever
        // the user's umask said, and a group-readable file replaced in place
        // silently stopped being readable by the group. Asking for 0666 puts
        // the umask back in charge, exactly as `File::create` would.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o666));
        }
        Ok(Self {
            staged: Some(builder.tempfile_in(parent)?),
            destination: destination.to_path_buf(),
            replace,
        })
    }
}

/// Whether nothing at all -- no file, no directory, not even a dangling
/// symlink -- is at `path`. Anything else, and any doubt, counts as present.
fn destination_is_absent(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == io::ErrorKind::NotFound)
}

impl Write for AtomicFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.staged
            .as_mut()
            .ok_or_else(|| io::Error::other("file already published"))?
            .write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        let Some(mut file) = self.staged.take() else {
            return Ok(());
        };
        file.flush()?;
        if self.replace {
            return file
                .persist(&self.destination)
                .map(|_| ())
                .map_err(|e| e.error);
        }
        match file.persist_noclobber(&self.destination) {
            Ok(_) => Ok(()),
            // `persist_noclobber` needs `RENAME_NOREPLACE` or, failing that, a
            // hard link, and exFAT, FAT and many SMB shares offer neither: on
            // those it fails for an *absent* destination too, after every byte
            // has arrived, and the user is told "operation not supported" with
            // nothing to suggest that replacing would have worked. The errno is
            // not what decides the retry -- it is ENOTSUP on macOS and EPERM
            // from `link` on Linux, and `ErrorKind` has no stable name for
            // either on this crate's rust-version. The question the flag was
            // asking is, so it is asked directly: anything already at the
            // destination, a dangling symlink included, keeps the refusal, and
            // only a destination that does not exist is retried with a plain
            // rename, accepting the window between the check and the rename on
            // the filesystems that cannot close it.
            Err(e) if destination_is_absent(&self.destination) => e
                .file
                .persist(&self.destination)
                .map(|_| ())
                .map_err(|e| e.error),
            Err(e) => Err(e.error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_preserves_the_destination_and_removes_staging() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        {
            let mut f = AtomicFile::new(&path, true).unwrap();
            f.write_all(b"partial").unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn replacement_is_explicit_and_publication_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let mut f = AtomicFile::new(&path, false).unwrap();
        f.write_all(b"new").unwrap();
        assert!(f.flush().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        let mut f = AtomicFile::new(&path, true).unwrap();
        f.write_all(b"complete").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        f.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete");
    }

    /// The retry without the no-clobber flag is only for a destination that
    /// is not there at all. The existence test has to see what `rename` sees,
    /// and `rename` does not follow a symlink at the destination.
    #[test]
    fn only_a_missing_destination_counts_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(destination_is_absent(&dir.path().join("nothing")));
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(!destination_is_absent(&file));
        assert!(!destination_is_absent(dir.path()));
        #[cfg(unix)]
        {
            let dangling = dir.path().join("dangling");
            std::os::unix::fs::symlink("nowhere", &dangling).unwrap();
            assert!(!destination_is_absent(&dangling));
        }
    }

    /// A dangling symlink is an existing destination: non-replacing
    /// publication keeps refusing it rather than renaming over it.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link");
        std::os::unix::fs::symlink("nowhere", &path).unwrap();
        let mut f = AtomicFile::new(&path, false).unwrap();
        f.write_all(b"new").unwrap();
        assert!(f.flush().is_err());
        assert!(std::fs::symlink_metadata(&path).unwrap().is_symlink());
        // The staging file is gone, so the directory holds only the link.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// A published file gets the mode `File::create` would have given it,
    /// whatever the umask is, not the 0600 of the staging file.
    #[cfg(unix)]
    #[test]
    fn published_files_follow_the_umask_like_any_other() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let published = dir.path().join("published");
        let mut f = AtomicFile::new(&published, false).unwrap();
        f.write_all(b"doc").unwrap();
        f.flush().unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"doc").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&published), mode(&plain));
    }
}
