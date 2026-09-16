//! Native metadata-only file offers; contents are requested only on use.
mod fetch;
pub use fetch::{Fetch, FetchError, FetchReply, FETCH_IDLE};
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Files;
#[cfg(any(not(target_os = "linux"), test))]
mod source;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Files;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(test, allow(dead_code))]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Files;

#[cfg(any(target_os = "macos", test))]
mod dav;

use std::path::Path;

/// Removes staging roots left behind by client processes that no longer
/// exist. Each client keeps its offers under `<parent>/<prefix><pid>`; a
/// process that quits without dropping them leaves the directory and, on
/// macOS, a WebDAV volume mounted inside it with nothing answering, because
/// Cmd-Q exits without unwinding. Call once at start, before the first offer.
///
/// A root whose process is still running, or belongs to another user, is
/// left alone. On Windows nothing is removed: this crate has no way to ask
/// whether a pid is alive there, and the mount leak this exists for is
/// macOS's.
pub fn sweep_stale_staging(parent: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(prefix))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() || process_exists(pid) {
            continue;
        }
        let path = entry.path();
        if !release_mounts(&path) {
            log::warn!(
                "a copied-files volume under {} is still mounted; left for the next start",
                path.display()
            );
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => log::info!("removed stale clipboard staging {}", path.display()),
            Err(e) => log::warn!(
                "could not remove stale clipboard staging {}: {e}",
                path.display()
            ),
        }
    }
}

#[cfg(target_os = "macos")]
fn release_mounts(root: &Path) -> bool {
    macos::unmount_below(root)
}
#[cfg(not(target_os = "macos"))]
fn release_mounts(_root: &Path) -> bool {
    true
}

/// Signal 0 checks without sending. EPERM means the process exists but is
/// someone else's, which for the purpose of leaving it alone is the same
/// answer; 0 and negative values address process groups, so they never get
/// as far as the call.
#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: kill with signal 0 performs only the existence and permission
    // checks and delivers nothing.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
#[cfg(not(unix))]
fn process_exists(_pid: u32) -> bool {
    true
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn staging_of_dead_processes_is_swept_and_the_rest_is_kept() {
        let parent = tempfile::tempdir().unwrap();
        // A reaped child's pid is free, so a root named after it is stale.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let stale = parent.path().join(format!("stage-{dead}"));
        std::fs::create_dir_all(stale.join("paste-abc/files")).unwrap();
        std::fs::write(stale.join("paste-abc/0"), b"cached").unwrap();
        let live = parent.path().join(format!("stage-{}", std::process::id()));
        let other = parent.path().join("other-1");
        let unparsable = parent.path().join("stage-x");
        for dir in [&live, &other, &unparsable] {
            std::fs::create_dir(dir).unwrap();
        }
        super::sweep_stale_staging(parent.path(), "stage-");
        assert!(!stale.exists());
        assert!(live.is_dir() && other.is_dir() && unparsable.is_dir());
    }
}
