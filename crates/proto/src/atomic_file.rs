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
        Ok(Self {
            staged: Some(
                tempfile::Builder::new()
                    .prefix(".lynxrdp-transfer-")
                    .tempfile_in(parent)?,
            ),
            destination: destination.to_path_buf(),
            replace,
        })
    }
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
        let result = if self.replace {
            file.persist(&self.destination)
        } else {
            file.persist_noclobber(&self.destination)
        };
        result.map(|_| ()).map_err(|e| e.error)
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
}
