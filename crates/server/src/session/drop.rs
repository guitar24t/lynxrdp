//! Stage incoming drops privately, then let the native target choose the destination.
use crate::x11::{
    drop::{DropSource, Target},
    XDisplay,
};
use anyhow::{bail, Result};
use lynxrdp_proto::{
    clipboard_batch::ClipBatch,
    transfer::{safe_relative_path, TransferManager},
    FileEntry, Message,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
};

struct Job {
    id: u64,
    target: Target,
    batch: ClipBatch,
    roots: Vec<PathBuf>,
}
pub(super) struct Drops {
    source: DropSource,
    jobs: VecDeque<Job>,
    pub staging: HashMap<u64, PathBuf>,
    root: PathBuf,
    serial: u64,
}
impl Drops {
    pub fn new(display: Arc<XDisplay>, root: PathBuf) -> Result<Self> {
        Ok(Self {
            source: DropSource::new(display)?,
            jobs: VecDeque::new(),
            staging: HashMap::new(),
            root,
            serial: 0,
        })
    }
    pub fn begin(&mut self, id: u64, x: u16, y: u16, files: Vec<FileEntry>) -> Result<()> {
        if self.jobs.len() + usize::from(self.source.busy()) >= 4 {
            bail!("Please wait for the current drops to finish.");
        }
        let target = self.source.target(x, y)?;
        self.serial += 1;
        let dir = self.root.join(self.serial.to_string());
        let (paths, roots) = plan(id, &dir, files)?;
        self.jobs.push_back(Job {
            id,
            target,
            batch: ClipBatch::with_paths(dir, paths),
            roots,
        });
        Ok(())
    }
    pub fn settle(&mut self, id: u64, ok: bool) {
        let path = self.staging.remove(&id).filter(|_| ok);
        for job in &mut self.jobs {
            if job.batch.resolve(id, path.clone()) {
                break;
            }
        }
    }
    pub fn event(&mut self, event: &x11rb::protocol::Event) -> Vec<Message> {
        match self.source.event(event) {
            Ok(result) => result.into_iter().map(result_message).collect(),
            Err(e) => self
                .source
                .fail(&format!("Could not deliver the drop: {e:#}"))
                .into_iter()
                .map(result_message)
                .collect(),
        }
    }
    pub fn pump(&mut self, transfers: &mut TransferManager) -> Vec<Message> {
        let mut out: Vec<_> = self.source.poll().into_iter().map(result_message).collect();
        // One batch at a time bounds disk/network work regardless of selection size.
        if let Some(job) = self.jobs.front_mut() {
            while let Some((path, dest, slot)) = job.batch.next_request() {
                let id = transfers.next_id();
                transfers.expect(id);
                job.batch.requested(slot, id);
                self.staging.insert(id, dest);
                out.push(Message::FileRequest { id, path });
            }
        }
        if !self.source.busy() && self.jobs.front().is_some_and(|j| j.batch.done()) {
            let job = self.jobs.pop_front().unwrap();
            let total = job.batch.total();
            let complete = job.batch.into_files().len() == total;
            let result = if complete {
                self.source.start(job.id, job.target, &job.roots)
            } else {
                Err(anyhow::anyhow!(
                    "Some files could not be received. The drop was cancelled; please retry."
                ))
            };
            if let Err(e) = result {
                self.source.cancel();
                out.push(Message::FileDropResult {
                    id: job.id,
                    ok: false,
                    reason: e.to_string(),
                });
            }
        }
        out
    }
    pub fn reset(&mut self) {
        self.source.cancel();
        self.jobs.clear();
        self.staging.clear();
    }
}
fn result_message((id, ok, reason): (u64, bool, String)) -> Message {
    Message::FileDropResult { id, ok, reason }
}

type Paths = Vec<(String, PathBuf)>;
fn plan(id: u64, dir: &std::path::Path, files: Vec<FileEntry>) -> Result<(Paths, Vec<PathBuf>)> {
    if files.is_empty() || files.len() > lynxrdp_proto::transfer::MAX_FILE_LIST {
        bail!("Invalid drop file count");
    }
    let prefix = format!("{id}/");
    let mut names = HashSet::new();
    let mut roots = HashSet::new();
    let mut paths = Vec::new();
    for file in files {
        let name = file
            .path
            .strip_prefix(&prefix)
            .ok_or_else(|| anyhow::anyhow!("Invalid drop path"))?;
        let safe = safe_relative_path(name)
            .filter(|s| s == name)
            .ok_or_else(|| anyhow::anyhow!("Unsafe drop path"))?;
        if !names.insert(safe.clone()) {
            bail!("Duplicate drop path");
        }
        roots.insert(dir.join(safe.split('/').next().unwrap()));
        paths.push((file.path, dir.join(safe)));
    }
    for name in &names {
        for (index, _) in name.match_indices('/') {
            if names.contains(&name[..index]) {
                bail!("Conflicting file and folder names");
            }
        }
    }
    let mut roots: Vec<_> = roots.into_iter().collect();
    roots.sort();
    Ok((paths, roots))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn files(names: &[&str]) -> Vec<FileEntry> {
        names
            .iter()
            .map(|s| FileEntry {
                path: s.to_string(),
                size: 10,
            })
            .collect()
    }
    #[test]
    fn preserves_tree_and_offers_only_roots() {
        let dir = std::path::Path::new("/private/staging");
        let (paths, roots) = plan(
            7,
            dir,
            files(&["7/Folder/a.txt", "7/Folder/nested/b.txt", "7/c.txt"]),
        )
        .unwrap();
        assert_eq!(paths[1].1, dir.join("Folder/nested/b.txt"));
        assert_eq!(roots, vec![dir.join("Folder"), dir.join("c.txt")]);
    }
    #[test]
    fn rejects_traversal_duplicates_and_file_folder_collisions() {
        for names in [
            vec!["7/../secret"],
            vec!["8/file"],
            vec!["7//absolute"],
            vec!["7/a", "7/a"],
            vec!["7/a", "7/a/b"],
            vec![],
        ] {
            assert!(
                plan(7, std::path::Path::new("/private/staging"), files(&names)).is_err(),
                "{names:?}"
            );
        }
    }
}
