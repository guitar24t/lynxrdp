use anyhow::{Context, Result};
use crossbeam_channel::{bounded, Receiver, Sender};
use lynxrdp_proto::{
    clipboard_batch::{safe_file_name, unique_name},
    FileEntry,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
pub struct Fetch {
    pub remote: String,
    pub destination: PathBuf,
    pub result: Sender<Option<PathBuf>>,
}
pub(crate) struct Source {
    pub files: Vec<FileEntry>,
    pub names: Vec<String>,
    pub directory: tempfile::TempDir,
    requests: Sender<Fetch>,
    cached: Mutex<Vec<Option<PathBuf>>>,
}
impl Source {
    pub fn new(parent: &Path, files: &[FileEntry]) -> Result<(Arc<Self>, Receiver<Fetch>)> {
        anyhow::ensure!(
            !files.is_empty() && files.len() <= 4096,
            "Invalid clipboard selection"
        );
        let directory = tempfile::Builder::new()
            .prefix("paste-")
            .tempdir_in(parent)?;
        let (tx, rx) = bounded(8);
        let mut taken = HashSet::new();
        let names = files
            .iter()
            .map(|f| {
                unique_name(
                    &mut taken,
                    &safe_file_name(
                        &lynxrdp_proto::urilist::base_name(Path::new(&f.path))
                            .unwrap_or_else(|| "file".into()),
                    ),
                )
            })
            .collect();
        Ok((
            Arc::new(Self {
                files: files.to_vec(),
                names,
                directory,
                requests: tx,
                cached: Mutex::new(vec![None; files.len()]),
            }),
            rx,
        ))
    }
    pub fn contents(&self, index: usize) -> Result<PathBuf> {
        let file = self
            .files
            .get(index)
            .context("Invalid clipboard file index")?;
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| anyhow::anyhow!("Clipboard worker failed"))?;
        if let Some(path) = &cached[index] {
            return Ok(path.clone());
        }
        let (tx, rx) = bounded(1);
        self.requests
            .send(Fetch {
                remote: file.path.clone(),
                destination: self.directory.path().join(index.to_string()),
                result: tx,
            })
            .context("Clipboard offer expired")?;
        let path = rx
            .recv_timeout(Duration::from_secs(300))
            .context("Clipboard transfer timed out")?
            .context("File could not be transferred")?;
        anyhow::ensure!(
            std::fs::metadata(&path)?.len() == file.size,
            "Source file changed after Copy"
        );
        cached[index] = Some(path.clone());
        Ok(path)
    }
}
