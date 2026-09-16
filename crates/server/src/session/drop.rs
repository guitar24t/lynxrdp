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
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// How long a delivered drop's staging stays after the XDND target has
/// acknowledged it.
///
/// `XdndFinished` says the target has read the URI list, not that it has
/// finished with the files: a file manager copies them afterwards, for as
/// long as that takes, straight from where they are staged. There is no
/// message for "done copying", so the staging is kept for a while and
/// removed on a later pump. Generous, because removing it early loses the
/// tail of somebody's copy; bounded, because before this nothing removed it
/// at all and a week-long session held every drop it had ever received.
const DELIVERED_STAGING_GRACE: Duration = Duration::from_secs(10 * 60);

/// How long a copy into the Desktop folder may take before the drop is failed.
///
/// A copy into the user's home has no natural bound -- a large drop onto a
/// slow home is legitimately slow -- but while one runs every later drop
/// queues behind it, and a copy wedged on a mount that stopped answering never
/// finishes at all. Ten minutes is longer than anyone waits at the screen for
/// a drop, and short enough that the session gets its drops back the same day.
const DESKTOP_DELIVERY_DEADLINE: Duration = Duration::from_secs(10 * 60);

struct Job {
    id: u64,
    target: Target,
    batch: ClipBatch,
    roots: Vec<PathBuf>,
    /// Where this drop's files are staged; released with the job.
    dir: PathBuf,
}
pub(super) struct Drops {
    source: DropSource,
    /// The drop being delivered by XDND, with its staging directory.
    delivering: Option<(u64, PathBuf)>,
    /// The drop being copied into the Desktop folder, with its staging.
    desktop: Option<(super::desktop_drop::Delivery, PathBuf)>,
    jobs: VecDeque<Job>,
    pub staging: HashMap<u64, PathBuf>,
    root: PathBuf,
    serial: u64,
    retired: Retired,
}
impl Drops {
    pub fn new(display: Arc<XDisplay>, root: PathBuf) -> Result<Self> {
        Ok(Self {
            source: DropSource::new(display)?,
            delivering: None,
            desktop: None,
            jobs: VecDeque::new(),
            staging: HashMap::new(),
            root,
            serial: 0,
            retired: Retired::default(),
        })
    }
    pub fn begin(&mut self, id: u64, x: u16, y: u16, files: Vec<FileEntry>) -> Result<()> {
        if self.jobs.len() + usize::from(self.source.busy()) + usize::from(self.desktop.is_some())
            >= 4
        {
            bail!("Please wait for the current drops to finish.");
        }
        let target = self.source.target(x, y)?;
        self.serial += 1;
        let dir = self.root.join(self.serial.to_string());
        let (paths, roots) = plan(id, &dir, files)?;
        self.jobs.push_back(Job {
            id,
            target,
            batch: ClipBatch::with_paths(paths),
            roots,
            dir,
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
        let result = match self.source.event(event) {
            Ok(result) => result,
            Err(e) => self
                .source
                .fail(&format!("Could not deliver the drop: {e:#}")),
        };
        self.delivered(result).into_iter().collect()
    }
    /// The reply for an XDND delivery that has ended, releasing its staging.
    ///
    /// Retired rather than removed, whatever the outcome: a target that did
    /// not answer `XdndFinished` in time may still be reading the files.
    fn delivered(&mut self, result: Option<(u64, bool, String)>) -> Option<Message> {
        let (id, ok, reason) = result?;
        if self
            .delivering
            .as_ref()
            .is_some_and(|(active, _)| *active == id)
        {
            let (_, dir) = self.delivering.take().unwrap();
            self.retired.retire(dir, Instant::now());
        }
        Some(Message::FileDropResult { id, ok, reason })
    }
    pub fn pump(&mut self, transfers: &mut TransferManager) -> Vec<Message> {
        let now = Instant::now();
        self.retired.prune(now);
        let mut out = Vec::new();
        let polled = self.source.poll();
        out.extend(self.delivered(polled));
        if let Some((delivery, _)) = &self.desktop {
            let result = match delivery.result.try_recv() {
                Ok(result) => Some(result),
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("Desktop copy worker stopped")))
                }
                Err(crossbeam_channel::TryRecvError::Empty)
                    if now.duration_since(delivery.started) >= DESKTOP_DELIVERY_DEADLINE =>
                {
                    Some(Err(anyhow::anyhow!(
                        "Copying to the Desktop folder did not finish in time"
                    )))
                }
                Err(crossbeam_channel::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                out.push(Message::FileDropResult {
                    id: delivery.id,
                    ok: result.is_ok(),
                    reason: match result {
                        Ok(()) => "Files copied to your Desktop folder.".into(),
                        Err(e) => format!(
                            "Desktop copy failed: {e:#}. Check the Desktop folder before retrying."
                        ),
                    },
                });
                // The copy read from the staging and is over -- or, past the
                // deadline, is abandoned: dropping the delivery cancels it.
                let (_, dir) = self.desktop.take().unwrap();
                Retired::discard(&dir);
            }
        }
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
        if !self.source.busy()
            && self.desktop.is_none()
            && self.jobs.front().is_some_and(|j| j.batch.done())
        {
            let job = self.jobs.pop_front().unwrap();
            let total = job.batch.total();
            let complete = job.batch.into_files().len() == total;
            let result = if complete {
                if job.target.desktop {
                    self.source.validate(job.target).and_then(|()| {
                        let delivery = super::desktop_drop::start(job.id, job.roots)?;
                        self.desktop = Some((delivery, job.dir.clone()));
                        Ok(())
                    })
                } else {
                    let started = self.source.start(job.id, job.target, &job.roots);
                    if started.is_ok() {
                        self.delivering = Some((job.id, job.dir.clone()));
                    }
                    started
                }
            } else {
                Err(anyhow::anyhow!(
                    "Some files could not be received. The drop was cancelled; please retry."
                ))
            };
            if let Err(e) = result {
                self.source.cancel();
                // Never reached a target, so nothing can be reading it.
                Retired::discard(&job.dir);
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
        if let Some((_, dir)) = self.desktop.take() {
            Retired::discard(&dir);
        }
        self.source.cancel();
        if let Some((_, dir)) = self.delivering.take() {
            self.retired.retire(dir, Instant::now());
        }
        for job in self.jobs.drain(..) {
            Retired::discard(&job.dir);
        }
        self.staging.clear();
    }
}
/// Staging directories of drops that are over, waiting to be removed.
///
/// One that never reached a target goes at once; one an XDND target took
/// waits out [`DELIVERED_STAGING_GRACE`] first. Removal failures are not
/// reported: the directory may never have been created (the file worker
/// creates it on the first file), and a leftover costs disk, not correctness.
#[derive(Default)]
struct Retired {
    dirs: Vec<(Instant, PathBuf)>,
}
impl Retired {
    fn discard(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
    fn retire(&mut self, dir: PathBuf, now: Instant) {
        self.dirs.push((now, dir));
    }
    fn prune(&mut self, now: Instant) {
        self.dirs.retain(|(retired_at, dir)| {
            if now.duration_since(*retired_at) < DELIVERED_STAGING_GRACE {
                return true;
            }
            Self::discard(dir);
            false
        });
    }
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
    #[test]
    fn retired_staging_waits_out_the_grace_and_discarded_staging_goes_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let taken = tmp.path().join("1");
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("file"), b"copying").unwrap();
        let refused = tmp.path().join("2");
        std::fs::create_dir(&refused).unwrap();
        Retired::discard(&refused);
        assert!(!refused.exists());
        // A drop the file worker never got as far as creating.
        Retired::discard(&tmp.path().join("3"));
        let mut retired = Retired::default();
        let t0 = Instant::now();
        retired.retire(taken.clone(), t0);
        retired.prune(t0 + DELIVERED_STAGING_GRACE - Duration::from_secs(1));
        assert!(
            taken.join("file").exists(),
            "removed while a target may still be copying"
        );
        retired.prune(t0 + DELIVERED_STAGING_GRACE);
        assert!(!taken.exists());
        assert!(retired.dirs.is_empty());
    }
}
