//! Putting a downloaded release where the running one is.
//!
//! Four shapes, because the three platforms disagree about what "the
//! application" is: a file on Linux, a directory on macOS, a file that cannot
//! be deleted while it runs on Windows, and -- when Windows put it in Program
//! Files -- something only an installer with an administrator manifest can
//! touch at all.
//!
//! Two rules hold across all of them.
//!
//! **The new build is fully in place before the old one stops existing.** Every
//! swap here unpacks into a staging name *on the same filesystem as the
//! target*, and only then renames. A rename within a filesystem either
//! happens or does not, so an interrupted update leaves a working
//! application and some rubbish beside it, never half an executable.
//!
//! **Nothing trusts the archive's own paths.** The tarball and the zip are
//! checksummed downloads from a release, not hostile input, but an entry
//! called `../../.ssh/authorized_keys` costs one function to refuse and would
//! cost rather more to explain. [`safe_relative`] is that function and
//! everything unpacking anything goes through it.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// How this build gets replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// One executable file, renamed over itself. Linux, and a macOS binary
    /// that is not inside an application bundle.
    Binary { target: PathBuf },
    /// A macOS `.app`, replaced whole.
    Bundle { bundle: PathBuf },
    /// A Windows executable, which cannot be deleted while it is running but
    /// can be renamed out of the way.
    WindowsExe { target: PathBuf },
    /// Hand the job to the NSIS installer, the only part of this that can
    /// ask for administrator rights.
    WindowsInstaller,
}

/// The name the release archives give the executable.
#[cfg(windows)]
const EXE_NAME: &str = "lynxrdp.exe";
#[cfg(not(windows))]
const EXE_NAME: &str = "lynxrdp";

/// The application bundle inside the macOS archive.
#[cfg(unix)]
const BUNDLE_NAME: &str = "LynxRDP.app";

/// The directory that has to be writable for `exe` to be replaced.
///
/// For a macOS application that is the directory *containing* the bundle,
/// because the swap renames the whole `.app` into place beside itself.
pub fn install_dir(exe: &Path) -> Option<PathBuf> {
    match super::bundle_of(exe) {
        Some(bundle) => bundle.parent().map(Path::to_path_buf),
        None => exe.parent().map(Path::to_path_buf),
    }
}

/// Whether this user can actually replace the application.
///
/// By writing a file, not by reading permission bits. The bits describe an
/// intention; a read-only mount, an ACL, a container's overlay or macOS's own
/// protection of `/Applications` for a non-admin user describe what will
/// happen when the rename is attempted. The probe is the same question the
/// update will ask, asked early enough to answer it in a sentence instead of
/// half way through.
pub fn can_write(exe: &Path) -> bool {
    let Some(dir) = install_dir(exe) else {
        return false;
    };
    let probe = dir.join(format!(".lynxrdp-write-test-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"lynxrdp").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// A path from inside an archive, refused unless it stays inside.
///
/// Absolute paths, drive letters, `..` and empty names are all rejected
/// rather than sanitised, because every one of them means the archive is not
/// the one we think we downloaded, and quietly repairing it would hide that.
pub fn safe_relative(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(p) => out.push(p),
            // A trailing "." is harmless and tar writes them; skip it.
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Drop the `lynxrdp-0.1.0-linux-x86_64/` wrapper the archives put
/// everything inside.
///
/// Returns `None` for the directory entry itself, which has nothing under it
/// to extract.
pub fn strip_top_level(path: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    parts.next()?;
    let rest: PathBuf = parts.collect();
    (!rest.as_os_str().is_empty()).then_some(rest)
}

/// A staging name beside `path`, distinct per process so two launchers
/// updating at once cannot collide.
fn staging(dir: &Path, name: &str, what: &str) -> PathBuf {
    dir.join(format!(".{name}.{what}-{}", std::process::id()))
}

/// Put the contents of `archive` in place.
pub fn apply(plan: &Plan, archive: &Path) -> Result<()> {
    match plan {
        Plan::Binary { target } => replace_binary(target, archive),
        Plan::Bundle { bundle } => replace_bundle(bundle, archive),
        Plan::WindowsExe { target } => replace_windows_exe(target, archive),
        // Handled by the caller, which has to leave afterwards.
        Plan::WindowsInstaller => bail!("the installer is run rather than unpacked"),
    }
}

// ------------------------------------------------------------------ unix

#[cfg(unix)]
mod unix {
    use super::*;
    use std::fs::Permissions;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    /// Open the `.tar.gz` for reading.
    pub fn open(archive: &Path) -> Result<tar::Archive<flate2::read::GzDecoder<std::fs::File>>> {
        let file = std::fs::File::open(archive)
            .with_context(|| format!("opening {}", archive.display()))?;
        Ok(tar::Archive::new(flate2::read::GzDecoder::new(file)))
    }

    /// Pull one file out of the archive by the path it has *inside* the
    /// version-named top-level directory.
    pub fn extract_file(archive: &Path, wanted: &Path) -> Result<(Vec<u8>, u32)> {
        let mut tar = open(archive)?;
        for entry in tar.entries().context("reading the archive")? {
            let mut entry = entry.context("reading an archive entry")?;
            let path = entry
                .path()
                .context("an archive entry has no path")?
                .into_owned();
            let Some(path) = safe_relative(&path) else {
                bail!("the archive contains an unsafe path: {}", path.display());
            };
            let Some(inner) = strip_top_level(&path) else {
                continue;
            };
            if inner != wanted {
                continue;
            }
            // Regular files only, and this is not a formality: the macOS
            // archive carries a *symlink* called `lynxrdp` beside the bundle,
            // pointing at the executable inside it. A symlink entry has no
            // data, so taking it would write an empty file over a working
            // application and leave nothing to run.
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let mode = entry.header().mode().unwrap_or(0o755);
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("reading the new build out of the archive")?;
            return Ok((bytes, mode));
        }
        bail!("the archive does not contain {}", wanted.display())
    }

    /// Write `bytes` beside `target` and rename it over the top.
    ///
    /// Renaming over a running executable is allowed on Unix: the process
    /// keeps the inode it started from, and the next start gets the new one.
    /// That is why this needs no "restart to finish" dance and no leftovers.
    pub fn swap_file(target: &Path, bytes: &[u8], mode: u32) -> Result<()> {
        let dir = target
            .parent()
            .context("the application has no parent directory")?;
        let name = target
            .file_name()
            .and_then(|n| n.to_str())
            .context("the application has no file name")?;
        let new = staging(dir, name, "new");
        std::fs::write(&new, bytes).with_context(|| format!("writing {}", new.display()))?;
        // The mode from the archive, but executable regardless: a build that
        // arrives without its bit set would replace a working application
        // with one the desktop cannot start.
        let mode = (mode & 0o777) | 0o755;
        if let Err(e) = std::fs::set_permissions(&new, Permissions::from_mode(mode)) {
            let _ = std::fs::remove_file(&new);
            return Err(e).with_context(|| format!("setting the mode on {}", new.display()));
        }
        if let Err(e) = std::fs::rename(&new, target) {
            let _ = std::fs::remove_file(&new);
            return Err(e).with_context(|| format!("replacing {}", target.display()));
        }
        Ok(())
    }

    /// Unpack every entry under `subtree` into `into`.
    pub fn extract_subtree(archive: &Path, subtree: &Path, into: &Path) -> Result<usize> {
        let mut tar = open(archive)?;
        let mut count = 0;
        std::fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;
        for entry in tar.entries().context("reading the archive")? {
            let mut entry = entry.context("reading an archive entry")?;
            let path = entry
                .path()
                .context("an archive entry has no path")?
                .into_owned();
            let Some(path) = safe_relative(&path) else {
                bail!("the archive contains an unsafe path: {}", path.display());
            };
            let Some(inner) = strip_top_level(&path) else {
                continue;
            };
            let Ok(rest) = inner.strip_prefix(subtree) else {
                continue;
            };
            let dest = if rest.as_os_str().is_empty() {
                into.to_path_buf()
            } else {
                into.join(rest)
            };
            let kind = entry.header().entry_type();
            if kind.is_dir() {
                std::fs::create_dir_all(&dest)
                    .with_context(|| format!("creating {}", dest.display()))?;
                continue;
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            if kind.is_symlink() {
                // Kept, but only pointing inside the bundle: a link out of it
                // would be the archive reaching somewhere it was not invited.
                let link = entry
                    .link_name()
                    .context("reading a symlink from the archive")?
                    .context("a symlink in the archive has no target")?;
                if safe_relative(&link).is_none() {
                    bail!("the archive links outside itself: {}", link.display());
                }
                std::os::unix::fs::symlink(&link, &dest)
                    .with_context(|| format!("linking {}", dest.display()))?;
                count += 1;
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let mode = entry.header().mode().unwrap_or(0o644);
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .with_context(|| format!("reading {} from the archive", dest.display()))?;
            std::fs::write(&dest, &bytes).with_context(|| format!("writing {}", dest.display()))?;
            std::fs::set_permissions(&dest, Permissions::from_mode(mode & 0o777))
                .with_context(|| format!("setting the mode on {}", dest.display()))?;
            count += 1;
        }
        if count == 0 {
            bail!("the archive contains no {}", subtree.display());
        }
        Ok(count)
    }
}

#[cfg(unix)]
fn replace_binary(target: &Path, archive: &Path) -> Result<()> {
    // The Linux archives hold the binary at the top; the macOS one holds an
    // application, and a loose macOS binary is the one inside it.
    let (bytes, mode) = unix::extract_file(archive, Path::new(EXE_NAME)).or_else(|first| {
        unix::extract_file(
            archive,
            &Path::new(BUNDLE_NAME)
                .join("Contents")
                .join("MacOS")
                .join(EXE_NAME),
        )
        .map_err(|_| first)
    })?;
    unix::swap_file(target, &bytes, mode)
}

#[cfg(unix)]
fn replace_bundle(bundle: &Path, archive: &Path) -> Result<()> {
    let parent = bundle
        .parent()
        .context("the application bundle has no parent directory")?;
    let name = bundle
        .file_name()
        .and_then(|n| n.to_str())
        .context("the application bundle has no name")?;
    let new = staging(parent, name, "new");
    let old = staging(parent, name, "old");
    // A staging directory left by an interrupted attempt would otherwise
    // merge with this one.
    let _ = std::fs::remove_dir_all(&new);
    let _ = std::fs::remove_dir_all(&old);
    unix::extract_subtree(archive, Path::new(BUNDLE_NAME), &new).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&new);
    })?;
    // Move the old one aside rather than deleting it, so a failure at the
    // second step can put it back. The running process keeps its files
    // either way -- the bundle is renamed, not emptied.
    std::fs::rename(bundle, &old).with_context(|| format!("moving {} aside", bundle.display()))?;
    match std::fs::rename(&new, bundle) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&old);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&old, bundle);
            let _ = std::fs::remove_dir_all(&new);
            Err(e).with_context(|| format!("putting the new build at {}", bundle.display()))
        }
    }
}

#[cfg(unix)]
fn replace_windows_exe(_target: &Path, _archive: &Path) -> Result<()> {
    bail!("a Windows executable cannot be replaced from here")
}

// --------------------------------------------------------------- windows

#[cfg(windows)]
mod win {
    use super::*;
    use std::io::Read;

    /// Pull one file out of the `.zip` by its path inside the top-level
    /// directory.
    pub fn extract_file(archive: &Path, wanted: &Path) -> Result<Vec<u8>> {
        let file = std::fs::File::open(archive)
            .with_context(|| format!("opening {}", archive.display()))?;
        let mut zip =
            zip::ZipArchive::new(file).with_context(|| format!("reading {}", archive.display()))?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).context("reading a zip entry")?;
            if !entry.is_file() {
                continue;
            }
            // `enclosed_name` is the zip crate's own refusal of absolute and
            // escaping paths; `safe_relative` is ours, and both are cheap.
            let Some(path) = entry.enclosed_name().as_deref().and_then(safe_relative) else {
                bail!("the archive contains an unsafe path");
            };
            let Some(inner) = strip_top_level(&path) else {
                continue;
            };
            if inner != wanted {
                continue;
            }
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("reading the new build out of the archive")?;
            return Ok(bytes);
        }
        bail!("the archive does not contain {}", wanted.display())
    }
}

/// Replace a running `.exe`.
///
/// Windows will not let the file be deleted or written while it is running,
/// but it will let it be *renamed*: the lock is on the contents, not the
/// name. So the old one is moved aside, the new one takes its place, and the
/// old one is deleted if it can be -- which it cannot be until this process
/// exits, so [`sweep`] finishes the job on the next start.
#[cfg(windows)]
fn replace_windows_exe(target: &Path, archive: &Path) -> Result<()> {
    let dir = target
        .parent()
        .context("the application has no parent directory")?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .context("the application has no file name")?;
    let bytes = win::extract_file(archive, Path::new(EXE_NAME))?;
    let new = staging(dir, name, "new");
    let old = staging(dir, name, "old");
    let _ = std::fs::remove_file(&new);
    let _ = std::fs::remove_file(&old);
    std::fs::write(&new, &bytes).with_context(|| format!("writing {}", new.display()))?;
    if let Err(e) = std::fs::rename(target, &old) {
        let _ = std::fs::remove_file(&new);
        return Err(e).with_context(|| format!("moving {} aside", target.display()));
    }
    match std::fs::rename(&new, target) {
        Ok(()) => {
            // Expected to fail while we are the running image; sweep catches
            // it next time.
            let _ = std::fs::remove_file(&old);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&old, target);
            let _ = std::fs::remove_file(&new);
            Err(e).with_context(|| format!("putting the new build at {}", target.display()))
        }
    }
}

#[cfg(windows)]
fn replace_binary(_target: &Path, _archive: &Path) -> Result<()> {
    bail!("a Unix binary cannot be replaced from here")
}

#[cfg(windows)]
fn replace_bundle(_bundle: &Path, _archive: &Path) -> Result<()> {
    bail!("a macOS application bundle cannot be replaced from here")
}

/// Whether a leftover from an earlier update should be deleted.
///
/// Only our own staging names, and only in the directory the application is
/// in. Written as a predicate on the name so that the rule -- and its
/// refusal to match `lynxrdp.exe` itself -- can be tested without creating
/// files anywhere.
pub fn is_leftover(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else {
        return false;
    };
    let Some((stem, tail)) = rest.rsplit_once('-') else {
        return false;
    };
    // Case-insensitively: the macOS staging name is `.LynxRDP.app.old-<pid>`
    // and the Linux one is `.lynxrdp.new-<pid>`, and a sweep that missed one
    // of them would leave a whole application bundle behind.
    tail.chars().all(|c| c.is_ascii_digit())
        && !tail.is_empty()
        && (stem.ends_with(".old") || stem.ends_with(".new"))
        && stem.to_ascii_lowercase().starts_with("lynxrdp")
}

/// Delete what an earlier update could not.
///
/// On Windows the replaced executable cannot be removed until the process
/// running it exits, so it is left behind on purpose and swept up here at the
/// next start. Failures are ignored: this is tidying, and a file that is
/// still locked will be gone the time after.
pub fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_leftover(name) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Move the downloaded installer somewhere it will outlive this process.
///
/// The download lives in a temporary directory that is deleted when the
/// update thread finishes, and the installer has to still be there after we
/// exit -- which is the whole point of handing off to it.
///
/// The name is predictable, which would be a poor idea in a shared `/tmp`.
/// It is not one here: the only plan that runs an installer is the Windows
/// one, and `%TEMP%` there is inside the user's own profile.
pub fn keep_installer(archive: &Path, name: &str) -> Result<PathBuf> {
    let dest = std::env::temp_dir().join(name);
    let _ = std::fs::remove_file(&dest);
    std::fs::copy(archive, &dest)
        .with_context(|| format!("putting the installer at {}", dest.display()))?;
    Ok(dest)
}

/// Start the installer and leave it to the user.
///
/// Not silent, on purpose. The installer's manifest asks for administrator,
/// so Windows raises a UAC prompt naming an unknown publisher -- exactly what
/// the README warns about -- and a user who is about to be asked that should
/// be looking at a window that explains itself, not answering a prompt that
/// appeared from nothing. Its finish page offers to start LynxRDP again,
/// which is the restart.
pub fn run_installer(path: &Path) -> Result<()> {
    std::process::Command::new(path)
        .spawn()
        .with_context(|| format!("starting {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_may_not_reach_outside_itself() {
        // The download is checksummed against the release it came from, so
        // this is not the day's likeliest attack -- but "it cannot happen"
        // is a claim with a shelf life, and the check costs one function.
        assert_eq!(
            safe_relative(Path::new("lynxrdp-0.1.0/lynxrdp")),
            Some(PathBuf::from("lynxrdp-0.1.0/lynxrdp"))
        );
        assert_eq!(
            safe_relative(Path::new("./a/b")),
            Some(PathBuf::from("a/b"))
        );
        assert_eq!(safe_relative(Path::new("../../etc/passwd")), None);
        assert_eq!(safe_relative(Path::new("a/../../b")), None);
        assert_eq!(safe_relative(Path::new("/etc/passwd")), None);
        assert_eq!(safe_relative(Path::new("")), None);
        #[cfg(windows)]
        {
            assert_eq!(safe_relative(Path::new(r"C:\Windows\system32")), None);
            assert_eq!(safe_relative(Path::new(r"\\server\share\x")), None);
        }
    }

    #[test]
    fn the_version_named_directory_is_stripped() {
        // Every archive wraps its contents in one, and the name carries a
        // version we deliberately do not try to predict.
        assert_eq!(
            strip_top_level(Path::new("lynxrdp-0.1.0-linux-x86_64/lynxrdp")),
            Some(PathBuf::from("lynxrdp"))
        );
        assert_eq!(
            strip_top_level(Path::new(
                "lynxrdp-0.1.0-macos-aarch64/LynxRDP.app/Contents/Info.plist"
            )),
            Some(PathBuf::from("LynxRDP.app/Contents/Info.plist"))
        );
        // The wrapper itself has nothing under it.
        assert_eq!(
            strip_top_level(Path::new("lynxrdp-0.1.0-linux-x86_64")),
            None
        );
        assert_eq!(strip_top_level(Path::new("")), None);
    }

    #[test]
    fn only_our_own_leftovers_are_swept() {
        assert!(is_leftover(".lynxrdp.exe.old-1234"));
        assert!(is_leftover(".lynxrdp.exe.new-1"));
        assert!(is_leftover(".LynxRDP.app.old-99"));
        // The application itself, and anything else a user keeps beside it.
        assert!(!is_leftover("lynxrdp.exe"));
        assert!(!is_leftover("lynxrdp.exe.old-1234"), "no leading dot");
        assert!(!is_leftover(".lynxrdp.exe.old-"), "no pid");
        assert!(!is_leftover(".lynxrdp.exe.old-abc"));
        assert!(!is_leftover(".connections.toml.swp"));
        assert!(!is_leftover(".ssh"));
    }

    #[test]
    fn is_leftover_matches_the_names_staging_actually_makes() {
        // The sweep and the swap have to agree, and they are forty lines
        // apart; this is the assertion that keeps them together.
        let dir = Path::new("/x");
        for name in ["lynxrdp", "lynxrdp.exe", "LynxRDP.app"] {
            for what in ["new", "old"] {
                let path = staging(dir, name, what);
                let file = path.file_name().unwrap().to_str().unwrap();
                assert!(is_leftover(file), "{file}");
            }
        }
    }

    #[test]
    fn the_directory_that_has_to_be_writable_is_the_one_being_replaced() {
        // For a bundle it is the folder the .app sits in, not the folder the
        // executable sits in four levels down -- getting this wrong would
        // test /Applications/LynxRDP.app/Contents/MacOS for writability and
        // then rename /Applications/LynxRDP.app.
        assert_eq!(
            install_dir(Path::new(
                "/Applications/LynxRDP.app/Contents/MacOS/lynxrdp"
            )),
            Some(PathBuf::from("/Applications"))
        );
        assert_eq!(
            install_dir(Path::new("/usr/local/bin/lynxrdp")),
            Some(PathBuf::from("/usr/local/bin"))
        );
    }

    #[test]
    fn a_writable_directory_is_found_by_writing_to_it() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(EXE_NAME);
        std::fs::write(&exe, b"old").unwrap();
        assert!(can_write(&exe));
        // And the probe leaves nothing behind.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
        // Somewhere that does not exist cannot be written.
        assert!(!can_write(&dir.path().join("nowhere").join(EXE_NAME)));
    }

    #[test]
    fn the_sweep_leaves_the_application_alone() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join(EXE_NAME);
        std::fs::write(&keep, b"the application").unwrap();
        std::fs::write(dir.path().join("connections.toml"), b"x").unwrap();
        std::fs::write(dir.path().join(".lynxrdp.exe.old-1234"), b"x").unwrap();
        std::fs::create_dir(dir.path().join(".LynxRDP.app.old-77")).unwrap();
        sweep(dir.path());
        assert!(keep.exists());
        assert!(dir.path().join("connections.toml").exists());
        assert!(!dir.path().join(".lynxrdp.exe.old-1234").exists());
        assert!(!dir.path().join(".LynxRDP.app.old-77").exists());
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        /// Build a release tarball the way `package-client.sh` does.
        fn tarball(dir: &Path, top: &str, files: &[(&str, &[u8], u32)]) -> PathBuf {
            build(dir, top, files, &[])
        }

        /// The same, plus symlinks: `(name, target)`. The macOS archive has
        /// one of these beside the bundle and it is named `lynxrdp`, which is
        /// exactly the name the Linux archive gives the executable.
        fn build(
            dir: &Path,
            top: &str,
            files: &[(&str, &[u8], u32)],
            links: &[(&str, &str)],
        ) -> PathBuf {
            let path = dir.join("release.tar.gz");
            let file = std::fs::File::create(&path).unwrap();
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            for (name, target) in links {
                let mut header = tar::Header::new_gnu();
                header.set_size(0);
                header.set_mode(0o777);
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_link_name(target).unwrap();
                tar.append_data(&mut header, format!("{top}/{name}"), std::io::empty())
                    .unwrap();
            }
            for (name, bytes, mode) in files {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(*mode);
                header.set_cksum();
                tar.append_data(&mut header, format!("{top}/{name}"), *bytes)
                    .unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
            path
        }

        #[test]
        fn the_symlink_beside_a_macos_bundle_is_not_mistaken_for_the_build() {
            // `package-client.sh` puts `lynxrdp -> LynxRDP.app/Contents/MacOS/
            // lynxrdp` at the top of the macOS archive, for anyone who wants
            // the command line. It has the same name as the Linux
            // executable, and a symlink entry carries no data: taking it
            // would write an empty file over a working application.
            let dir = tempfile::tempdir().unwrap();
            let archive = build(
                dir.path(),
                "lynxrdp-0.1.0-macos-aarch64",
                &[(
                    "LynxRDP.app/Contents/MacOS/lynxrdp",
                    b"the real build",
                    0o755,
                )],
                &[("lynxrdp", "LynxRDP.app/Contents/MacOS/lynxrdp")],
            );
            let target = dir.path().join("lynxrdp");
            std::fs::write(&target, b"the old build").unwrap();
            apply(
                &Plan::Binary {
                    target: target.clone(),
                },
                &archive,
            )
            .unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"the real build");
        }

        #[test]
        fn a_linux_binary_is_replaced_in_place_and_stays_executable() {
            let dir = tempfile::tempdir().unwrap();
            let archive = tarball(
                dir.path(),
                "lynxrdp-0.1.0-linux-x86_64",
                &[
                    ("lynxrdp", b"the new build", 0o755),
                    ("README.md", b"not this", 0o644),
                ],
            );
            let target = dir.path().join("lynxrdp");
            std::fs::write(&target, b"the old build").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

            apply(
                &Plan::Binary {
                    target: target.clone(),
                },
                &archive,
            )
            .unwrap();

            assert_eq!(std::fs::read(&target).unwrap(), b"the new build");
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "must still be executable");
            // Nothing staged is left over.
            let names: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with('.'))
                .collect();
            assert!(names.is_empty(), "{names:?}");
        }

        #[test]
        fn a_build_that_arrives_without_its_executable_bit_still_starts() {
            // A tarball built on a filesystem that does not keep modes would
            // otherwise replace a working application with one the desktop
            // cannot launch, and the user would have no way to tell why.
            let dir = tempfile::tempdir().unwrap();
            let archive = tarball(
                dir.path(),
                "lynxrdp-0.1.0-linux-x86_64",
                &[("lynxrdp", b"new", 0o644)],
            );
            let target = dir.path().join("lynxrdp");
            std::fs::write(&target, b"old").unwrap();
            apply(
                &Plan::Binary {
                    target: target.clone(),
                },
                &archive,
            )
            .unwrap();
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }

        #[test]
        fn a_macos_application_is_replaced_whole() {
            let dir = tempfile::tempdir().unwrap();
            let archive = tarball(
                dir.path(),
                "lynxrdp-0.1.0-macos-aarch64",
                &[
                    ("LynxRDP.app/Contents/MacOS/lynxrdp", b"new binary", 0o755),
                    (
                        "LynxRDP.app/Contents/Info.plist",
                        b"<plist>new</plist>",
                        0o644,
                    ),
                    (
                        "LynxRDP.app/Contents/Resources/lynxrdp.icns",
                        b"icon",
                        0o644,
                    ),
                    // The symlink beside the bundle, which is not part of it.
                    ("lynxrdp", b"", 0o755),
                ],
            );
            let bundle = dir.path().join("LynxRDP.app");
            std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
            std::fs::write(bundle.join("Contents/MacOS/lynxrdp"), b"old binary").unwrap();
            std::fs::write(bundle.join("Contents/Info.plist"), b"<plist>old</plist>").unwrap();
            // A file only the old bundle had, to prove the bundle is
            // replaced rather than merged into.
            std::fs::write(bundle.join("Contents/stale"), b"x").unwrap();

            apply(
                &Plan::Bundle {
                    bundle: bundle.clone(),
                },
                &archive,
            )
            .unwrap();

            assert_eq!(
                std::fs::read(bundle.join("Contents/MacOS/lynxrdp")).unwrap(),
                b"new binary"
            );
            assert_eq!(
                std::fs::read(bundle.join("Contents/Info.plist")).unwrap(),
                b"<plist>new</plist>"
            );
            assert!(bundle.join("Contents/Resources/lynxrdp.icns").exists());
            assert!(
                !bundle.join("Contents/stale").exists(),
                "the old bundle must be replaced, not merged into"
            );
            let mode = std::fs::metadata(bundle.join("Contents/MacOS/lynxrdp"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
        }

        #[test]
        fn an_archive_without_the_build_in_it_leaves_the_old_one_alone() {
            // The failure that matters: the update must not delete a working
            // application on its way to discovering it has nothing to
            // install.
            let dir = tempfile::tempdir().unwrap();
            let archive = tarball(
                dir.path(),
                "lynxrdp-0.1.0-linux-x86_64",
                &[("README.md", b"only docs", 0o644)],
            );
            let target = dir.path().join("lynxrdp");
            std::fs::write(&target, b"the old build").unwrap();
            let err = apply(
                &Plan::Binary {
                    target: target.clone(),
                },
                &archive,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("does not contain"), "{err:#}");
            assert_eq!(std::fs::read(&target).unwrap(), b"the old build");

            let bundle = dir.path().join("LynxRDP.app");
            std::fs::create_dir_all(&bundle).unwrap();
            std::fs::write(bundle.join("keep"), b"still here").unwrap();
            assert!(apply(
                &Plan::Bundle {
                    bundle: bundle.clone()
                },
                &archive
            )
            .is_err());
            assert_eq!(std::fs::read(bundle.join("keep")).unwrap(), b"still here");
        }

        /// A tarball holding one entry whose name climbs out of the archive.
        ///
        /// The name is written into the header's bytes by hand because
        /// `tar::Builder` refuses to produce this: `set_path` rejects a `..`
        /// component outright. That refusal is a decent argument that the
        /// archives we actually publish can never contain one -- and no
        /// argument at all about an archive that did not come from us, which
        /// is what the extractor has to survive.
        fn escaping_tarball(dir: &Path, top: &str, escape: &str) -> PathBuf {
            let path = dir.join("escape.tar.gz");
            let file = std::fs::File::create(&path).unwrap();
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut tar = tar::Builder::new(gz);
            let body = b"nope";
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            let name = format!("{top}/{escape}");
            let gnu = header.as_gnu_mut().expect("a gnu header");
            gnu.name[..name.len()].copy_from_slice(name.as_bytes());
            // After the name, or it checksums the empty one.
            header.set_cksum();
            tar.append(&header, &body[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
            path
        }

        #[test]
        fn an_archive_that_climbs_out_of_itself_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let archive =
                escaping_tarball(dir.path(), "lynxrdp-0.1.0-linux-x86_64", "../../escaped");
            let target = dir.path().join("lynxrdp");
            std::fs::write(&target, b"old").unwrap();
            let err = apply(
                &Plan::Binary {
                    target: target.clone(),
                },
                &archive,
            )
            .unwrap_err();
            assert!(format!("{err:#}").contains("unsafe path"), "{err:#}");
            assert!(!dir.path().join("escaped").exists());
            assert_eq!(std::fs::read(&target).unwrap(), b"old");

            // And the same on the way into a bundle, which unpacks every
            // entry rather than looking for one.
            let bundle = dir.path().join("LynxRDP.app");
            std::fs::create_dir_all(&bundle).unwrap();
            let archive = escaping_tarball(
                dir.path(),
                "lynxrdp-0.1.0-macos-aarch64",
                "LynxRDP.app/../../escaped",
            );
            let err = apply(&Plan::Bundle { bundle }, &archive).unwrap_err();
            assert!(format!("{err:#}").contains("unsafe path"), "{err:#}");
            assert!(!dir.path().join("escaped").exists());
        }
    }

    #[cfg(windows)]
    mod windows_tests {
        use super::*;
        use std::io::Write;

        /// Build a release zip the way `package-client.sh` does.
        fn zipball(dir: &Path, top: &str, files: &[(&str, &[u8])]) -> PathBuf {
            let path = dir.join("release.zip");
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, bytes) in files {
                zip.start_file(format!("{top}/{name}"), options).unwrap();
                zip.write_all(bytes).unwrap();
            }
            zip.finish().unwrap();
            path
        }

        #[test]
        fn a_running_executable_is_renamed_aside_rather_than_written_over() {
            // Windows will not let a running image be written or deleted, but
            // it will let it be renamed. The old one is expected to still be
            // there afterwards -- `sweep` gets it at the next start -- and
            // that is what this pins down.
            let dir = tempfile::tempdir().unwrap();
            let archive = zipball(
                dir.path(),
                "lynxrdp-0.1.0-windows-x86_64",
                &[
                    ("lynxrdp.exe", b"the new build"),
                    ("README.md", b"not this"),
                ],
            );
            let target = dir.path().join("lynxrdp.exe");
            std::fs::write(&target, b"the old build").unwrap();
            apply(
                &Plan::WindowsExe {
                    target: target.clone(),
                },
                &archive,
            )
            .unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"the new build");
        }

        #[test]
        fn an_archive_without_the_build_in_it_leaves_the_old_one_alone() {
            let dir = tempfile::tempdir().unwrap();
            let archive = zipball(
                dir.path(),
                "lynxrdp-0.1.0-windows-x86_64",
                &[("README.md", b"only docs")],
            );
            let target = dir.path().join("lynxrdp.exe");
            std::fs::write(&target, b"the old build").unwrap();
            assert!(apply(
                &Plan::WindowsExe {
                    target: target.clone()
                },
                &archive
            )
            .is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"the old build");
        }

        #[test]
        fn an_archive_that_climbs_out_of_itself_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let archive = zipball(
                dir.path(),
                "lynxrdp-0.1.0-windows-x86_64",
                &[("../escaped.exe", b"nope")],
            );
            let target = dir.path().join("lynxrdp.exe");
            std::fs::write(&target, b"old").unwrap();
            assert!(apply(&Plan::WindowsExe { target }, &archive).is_err());
            assert!(!dir.path().join("escaped.exe").exists());
        }
    }
}
