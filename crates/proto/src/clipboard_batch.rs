//! Shared planning and settlement for clipboard file copies.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
/// Concurrent files in a clipboard batch.
pub const MAX_CONCURRENT_CLIPBOARD_FILES: usize = 8;
/// Leave room for collision suffixes within filesystem name limits.
pub const MAX_STAGED_NAME: usize = 200;
/// Reduce a name from the session to one every platform can create.
///
/// Sanitising happens on all three platforms, not under `cfg(windows)`, for
/// two reasons. The obvious one is that the staged file has to be creatable
/// here; the less obvious one is that the *result* has to be predictable,
/// because the caller deduplicates names and a rule that differs per platform
/// gives a different set of collisions per platform -- so the case that
/// silently overwrites a file would only ever appear on the platform nobody
/// tested on.
///
/// What is removed: the path separators and the characters Windows rejects
/// outright, control characters (a newline in a file name is legal on Linux
/// and a menace everywhere else), trailing dots and spaces, which Windows
/// strips when it resolves a name so that `report.` and `report` are the same
/// file, and the device names that are reserved whatever the extension.
pub fn safe_file_name(raw: &str) -> String {
    const FORBIDDEN: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    // `CON.txt` opens the console on Windows, not a file. The check is on the
    // part before the first dot with trailing spaces removed, which is how
    // Windows resolves them: `CON .txt` is the console too. The superscript
    // digits are reserved alongside the plain ones.
    const RESERVED: &[&str] = &[
        "CON",
        "PRN",
        "AUX",
        "NUL",
        "COM1",
        "COM2",
        "COM3",
        "COM4",
        "COM5",
        "COM6",
        "COM7",
        "COM8",
        "COM9",
        "COM\u{b9}",
        "COM\u{b2}",
        "COM\u{b3}",
        "LPT1",
        "LPT2",
        "LPT3",
        "LPT4",
        "LPT5",
        "LPT6",
        "LPT7",
        "LPT8",
        "LPT9",
        "LPT\u{b9}",
        "LPT\u{b2}",
        "LPT\u{b3}",
    ];

    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || FORBIDDEN.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();

    if name.len() > MAX_STAGED_NAME {
        // Keep the extension: it is what a file manager uses to decide what
        // the pasted file is, and losing it turns a spreadsheet into a blob.
        let ext = Path::new(&name)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .filter(|e| e.len() <= 16)
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let mut cut = MAX_STAGED_NAME - ext.len();
        while cut > 0 && !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
        name.push_str(&ext);
    }

    // After the truncation, not before: cutting a name can expose a dot that
    // was in the middle of it.
    let trimmed = name.trim_end_matches([' ', '.']);
    // This is also what rules out "." and "..", which trim to nothing.
    let mut name = if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    };
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        name.insert(0, '_');
    }
    name
}

/// Make `name` unique among the names already `taken`, recording it.
///
/// Two files called `notes.txt` from different directories in the session used
/// to be staged over each other -- the second `File::create` truncated the
/// first and the paste delivered one file where the user copied two, with no
/// error anywhere. Comparison is through [`collision_key`], so that the names
/// counted as clashing are the ones the target filesystems treat as one file.
pub fn unique_name(taken: &mut HashSet<String>, name: &str) -> String {
    if taken.insert(collision_key(name)) {
        return name.to_string();
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    // Terminates because `taken` is finite and every candidate is distinct.
    let mut n = 2u32;
    loop {
        let candidate = format!("{stem} ({n}){ext}");
        if taken.insert(collision_key(&candidate)) {
            return candidate;
        }
        n += 1;
    }
}

/// The form two names are compared in: case folded and canonically composed.
///
/// NTFS folds case, and a default APFS volume folds case *and* normalisation:
/// `café` spelled with U+00E9 and spelled `e` plus U+0301 are one file there,
/// and both spellings do occur in a Linux session, because macOS tools write
/// the decomposed form and most others the composed one. Two such names
/// published side by side reach the Mac as one entry and one of the files is
/// silently lost. Composition runs after folding, since folding can itself
/// produce a decomposed sequence.
fn collision_key(name: &str) -> String {
    name.to_lowercase().nfc().collect()
}

/// One clipboard file copy, from the session's list to the local clipboard.
///
/// A batch cannot be tracked by a count, which is what it was: the counter
/// only came down when a file arrived, so a single file that could not be
/// created locally left it stuck above zero and the copy was never published
/// at all. That is disproportionately a Windows failure -- `a:b.txt`,
/// `what?.png`, `CON` and a trailing dot are all ordinary names in an X11
/// session and none of them can be created on NTFS -- and the user's only
/// symptom was Ctrl+V returning their old clipboard.
///
/// Here every file has a slot and every transfer has an id, each id is
/// resolved exactly once whether it arrived or failed, and the batch finishes
/// when there is nothing left outstanding. What did not arrive is simply
/// missing from the published list.
pub struct ClipBatch {
    /// Not yet requested: (remote path, local destination, slot).
    queued: VecDeque<(String, PathBuf, usize)>,
    /// Requested and unresolved: transfer id to slot.
    live: HashMap<u64, usize>,
    /// Where each file landed, in the order the session listed them. Order is
    /// kept because it is the order the user selected, and a file manager
    /// shows a paste in the order it is given.
    slots: Vec<Option<PathBuf>>,
}

impl ClipBatch {
    /// Plan a copy: one slot per file, with a staged name that is safe on this
    /// platform and unique within the batch.
    pub fn new(dir: PathBuf, files: &[crate::FileEntry]) -> Self {
        let mut taken = HashSet::new();
        let mut queued = VecDeque::with_capacity(files.len());
        for (slot, f) in files.iter().enumerate() {
            let base = crate::urilist::base_name(Path::new(&f.path)).unwrap_or_default();
            let name = unique_name(&mut taken, &safe_file_name(&base));
            queued.push_back((f.path.clone(), dir.join(name), slot));
        }
        Self {
            queued,
            live: HashMap::new(),
            slots: vec![None; files.len()],
        }
    }

    /// Build a batch with caller-validated destinations, preserving directory trees.
    ///
    /// No staging directory is passed, unlike [`ClipBatch::new`]: the caller has
    /// already resolved every destination against one, so a second copy here
    /// would be a parameter the batch stores and never consults.
    pub fn with_paths(files: Vec<(String, PathBuf)>) -> Self {
        let count = files.len();
        Self {
            queued: files
                .into_iter()
                .enumerate()
                .map(|(slot, (remote, dest))| (remote, dest, slot))
                .collect(),
            live: HashMap::new(),
            slots: vec![None; count],
        }
    }

    /// How many files the session offered.
    pub fn total(&self) -> usize {
        self.slots.len()
    }

    /// The next file to request, if the concurrency limit leaves room.
    pub fn next_request(&mut self) -> Option<(String, PathBuf, usize)> {
        if self.live.len() >= MAX_CONCURRENT_CLIPBOARD_FILES {
            return None;
        }
        self.queued.pop_front()
    }

    /// Record the transfer id a request was given.
    pub fn requested(&mut self, slot: usize, id: u64) {
        self.live.insert(id, slot);
    }

    /// Resolve a transfer: `Some(path)` if it arrived, `None` if it failed.
    /// Returns whether the id belonged to this batch.
    pub fn resolve(&mut self, id: u64, path: Option<PathBuf>) -> bool {
        let Some(slot) = self.live.remove(&id) else {
            return false;
        };
        if let Some(p) = path {
            self.slots[slot] = Some(p);
        }
        true
    }

    /// Whether every file has arrived, failed or been given up on.
    pub fn done(&self) -> bool {
        self.queued.is_empty() && self.live.is_empty()
    }

    /// The files that actually landed, in the order they were copied.
    pub fn into_files(self) -> Vec<PathBuf> {
        self.slots.into_iter().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows strips the extension and then trailing spaces before it
    /// compares against the device names, so `CON .txt` is the console; and
    /// the superscript digits are device numbers too.
    #[test]
    fn reserved_device_names_are_caught_as_windows_resolves_them() {
        assert_eq!(safe_file_name("CON .txt"), "_CON .txt");
        assert_eq!(safe_file_name("con.txt"), "_con.txt");
        assert_eq!(safe_file_name("COM\u{b9}"), "_COM\u{b9}");
        assert_eq!(safe_file_name("lpt\u{b3}.log"), "_lpt\u{b3}.log");
        // A device name that is not a whole stem is an ordinary file.
        assert_eq!(safe_file_name("CONSOLE.txt"), "CONSOLE.txt");
        assert_eq!(safe_file_name("COM10"), "COM10");
    }

    /// The NFC and NFD spellings of one name are one file on a default APFS
    /// volume, so they have to collide here or one of the two is lost on the
    /// Mac without an error.
    #[test]
    fn unique_name_treats_normalisation_twins_as_a_collision() {
        let mut taken = HashSet::new();
        let nfc = "caf\u{e9}.txt";
        let nfd = "cafe\u{301}.txt";
        assert_eq!(unique_name(&mut taken, nfc), nfc);
        let second = unique_name(&mut taken, nfd);
        assert_ne!(second, nfd);
        assert!(second.ends_with(" (2).txt"), "{second:?}");
        // Case folding still applies on top of composition.
        assert!(unique_name(&mut taken, "CAF\u{c9}.txt").ends_with(" (3).txt"));
        // Names that only look alike are not collisions.
        assert_eq!(unique_name(&mut taken, "cafe.txt"), "cafe.txt");
    }

    /// The staged name comes from the same base-name rule on every platform,
    /// so a Windows client's native path does not become the file's name in
    /// a Linux session.
    #[test]
    fn batch_names_come_from_the_shared_base_name_rule() {
        let files = vec![
            crate::FileEntry {
                path: r"C:\Users\alice\Documents\report.pdf".into(),
                size: 1,
            },
            crate::FileEntry {
                path: "/home/alice/old/report.pdf".into(),
                size: 2,
            },
            crate::FileEntry {
                path: "/home/alice/..".into(),
                size: 3,
            },
        ];
        let mut batch = ClipBatch::new(PathBuf::from("/stage"), &files);
        let names: Vec<_> = std::iter::from_fn(|| batch.next_request())
            .map(|(_, dest, _)| dest.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["report.pdf", "report (2).pdf", "file"]);
    }
}
