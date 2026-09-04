//! Saved connections for the launcher.
//!
//! A small TOML file in the platform's usual configuration directory holding
//! everything needed to start a session, so a host is typed once and clicked
//! thereafter.
//!
//! Deliberately free of any UI, and free of any process spawning, so the
//! rules about what a profile means and what arguments it produces can be
//! tested without a window or a network.
//!
//! No credentials are stored here. SSH already owns authentication -- keys,
//! agents, `~/.ssh/config`, hardware tokens, prompts -- and duplicating any
//! of that into a config file of ours would be a step backwards. A profile
//! names an identity *file* at most, which is a path, not a secret.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// File name inside the configuration directory.
pub const FILE_NAME: &str = "connections.toml";

/// Most profiles we will keep. Guards against a corrupt or hostile file
/// making the launcher unusable.
pub const MAX_PROFILES: usize = 500;

/// Longest name, host or user we accept.
pub const MAX_FIELD: usize = 255;

/// How many rejected copies of the connections file we will keep before
/// refusing to make another. A hundred `.bad` files means something is wrong
/// that moving a hundred and first aside will not fix.
pub const MAX_ASIDE: u32 = 100;

/// Whether a flag is off, for `skip_serializing_if`.
///
/// Defaults are left out of the file rather than written as `= false` and
/// `= []`. The file is meant to be readable and hand-editable, and a wall of
/// unset fields buries the two or three lines that actually say anything.
fn is_false(value: &bool) -> bool {
    !*value
}

/// One saved connection.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Profile {
    /// What the user sees in the list. Unique within the store.
    pub name: String,
    /// Host name or address, or a `~/.ssh/config` alias.
    pub host: String,
    /// SSH user. Empty means whatever SSH would pick.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// SSH port. `None` means the default, or whatever the config says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh_port: Option<u16>,
    /// Identity file passed as `ssh -i`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<PathBuf>,
    /// Extra `-o key=value` options.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ssh_options: Vec<String>,
    /// LynxRDP port on the server's loopback interface.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_port: Option<u16>,
    /// Initial screen size. `None` uses the server's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<(u16, u16)>,
    /// Start the session fullscreen.
    #[serde(skip_serializing_if = "is_false")]
    pub fullscreen: bool,
    /// Let the remote screen follow the window size.
    pub dynamic_resize: bool,
    /// Synchronise the clipboard.
    pub clipboard: bool,
}

impl Profile {
    /// A new profile with the defaults the GUI should start from.
    ///
    /// `Default` cannot be used for this: `dynamic_resize` and `clipboard`
    /// are on unless turned off, and `bool::default()` is false.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            dynamic_resize: true,
            clipboard: true,
            ..Default::default()
        }
    }

    /// `user@host`, or just the host when no user is set.
    pub fn destination(&self) -> String {
        if self.user.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.user, self.host)
        }
    }

    /// What is wrong with this profile, if anything.
    ///
    /// Returned as a message for the editor to show rather than an error to
    /// propagate, because the caller is a form the user is still filling in.
    pub fn problem(&self) -> Option<String> {
        if self.name.trim().is_empty() {
            return Some("A name is required.".into());
        }
        if self.name.len() > MAX_FIELD {
            return Some(format!("The name must be under {MAX_FIELD} characters."));
        }
        if self.host.trim().is_empty() {
            return Some("A host is required.".into());
        }
        if self.host.len() > MAX_FIELD || self.user.len() > MAX_FIELD {
            return Some(format!(
                "The host and user must each be under {MAX_FIELD} characters."
            ));
        }
        // A host with whitespace would be split when it reaches ssh, and a
        // leading dash would be read as an option.
        if self.host.split_whitespace().count() != 1 {
            return Some("The host must not contain spaces.".into());
        }
        if self.host.starts_with('-') || self.user.starts_with('-') {
            return Some("The host and user must not start with '-'.".into());
        }
        if self.user.contains('@') || self.user.split_whitespace().count() > 1 {
            return Some("The user must not contain '@' or spaces.".into());
        }
        if self.ssh_port == Some(0) || self.remote_port == Some(0) {
            return Some("Ports must be between 1 and 65535.".into());
        }
        if let Some((w, h)) = self.size {
            if w == 0 || h == 0 {
                return Some("The screen size must be positive.".into());
            }
        }
        for option in &self.ssh_options {
            if option.trim().is_empty() {
                return Some("An SSH option is blank.".into());
            }
            if !option.contains('=') {
                return Some(format!("SSH option {option:?} should look like Key=value."));
            }
        }
        None
    }

    /// The command line that starts this session.
    ///
    /// Returned as separate arguments, never a shell string: the launcher
    /// spawns the binary directly, so a host or option containing a space or
    /// a quote cannot turn into extra arguments.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec![self.destination()];
        if let Some(port) = self.ssh_port {
            args.push("--port".into());
            args.push(port.to_string());
        }
        if let Some(identity) = &self.identity {
            args.push("--identity".into());
            args.push(identity.display().to_string());
        }
        for option in &self.ssh_options {
            args.push("--ssh-option".into());
            args.push(option.clone());
        }
        if let Some(port) = self.remote_port {
            args.push("--remote-port".into());
            args.push(port.to_string());
        }
        if let Some((w, h)) = self.size {
            args.push("--size".into());
            args.push(format!("{w}x{h}"));
        }
        if self.fullscreen {
            args.push("--fullscreen".into());
        }
        if !self.dynamic_resize {
            args.push("--no-dynamic-resize".into());
        }
        if !self.clipboard {
            args.push("--no-clipboard".into());
        }
        args
    }
}

/// The saved connections, in the order they are shown.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Profiles {
    #[serde(rename = "connection")]
    pub items: Vec<Profile>,
}

impl Profiles {
    /// Where the connections file lives on this platform.
    ///
    /// Honours the usual environment overrides so a test, or a user with an
    /// unusual setup, can point it elsewhere.
    pub fn default_path() -> Result<PathBuf> {
        let dir = config_dir().context("could not determine a configuration directory")?;
        Ok(dir.join(FILE_NAME))
    }

    /// Read the file. A missing file is an empty list, not an error: that is
    /// simply the state before the first connection is saved.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text)
                .with_context(|| format!("reading connections from {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Parse, rejecting a file that would make the launcher unusable.
    pub fn from_toml(text: &str) -> Result<Self> {
        let parsed: Self = toml::from_str(text).context("parsing connections")?;
        if parsed.items.len() > MAX_PROFILES {
            bail!(
                "{} saved connections is more than the {MAX_PROFILES} supported",
                parsed.items.len()
            );
        }
        Ok(parsed)
    }

    /// Serialise.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Write the file, creating the directory if needed.
    ///
    /// Written to a temporary file and renamed, so an interrupted save
    /// cannot leave a half-written file where the connections used to be.
    /// Two details make that actually true rather than merely intended:
    /// the temporary name is unique to this process, and its contents are
    /// on disk before the rename. `temp_path` and `write_durably` say what
    /// goes wrong without each.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let temporary = temp_path(path);
        if let Err(e) = write_durably(&temporary, self.to_toml().as_bytes()) {
            // Both error paths take the temporary with them. Correctness
            // does not depend on it -- the next save truncates whatever is
            // there -- but this is the user's configuration directory, and a
            // stray connections.toml.4711.new sitting next to the real file
            // is exactly the sort of thing that gets opened and edited by
            // mistake.
            let _ = std::fs::remove_file(&temporary);
            return Err(e).with_context(|| format!("writing {}", temporary.display()));
        }
        if let Err(e) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(e).with_context(|| format!("replacing {}", path.display()));
        }
        Ok(())
    }

    /// Position of a profile by name.
    pub fn position(&self, name: &str) -> Option<usize> {
        self.items.iter().position(|p| p.name == name)
    }

    /// Add a profile, or replace the one that has the same name.
    ///
    /// Returns whether it replaced an existing entry, which the caller uses
    /// to decide whether the list selection should move.
    pub fn upsert(&mut self, profile: Profile) -> bool {
        match self.position(&profile.name) {
            Some(index) => {
                self.items[index] = profile;
                true
            }
            None => {
                self.items.push(profile);
                false
            }
        }
    }

    /// Whether `name` belongs to some profile other than `excluding`.
    ///
    /// The name is the key: [`Self::upsert`] replaces the entry that has it.
    /// So an editor that saved onto a name already in use would not report a
    /// clash, it would quietly fold two connections into one and lose the
    /// host of whichever was there first. `excluding` is the name the editor
    /// started from, since saving a profile back under its own name is not a
    /// clash. Compared exactly, matching [`Self::position`]: a check that
    /// disagreed with the lookup would refuse saves that upsert would have
    /// treated as new.
    pub fn name_taken(&self, name: &str, excluding: Option<&str>) -> bool {
        self.items
            .iter()
            .any(|p| p.name == name && Some(p.name.as_str()) != excluding)
    }

    /// Remove by name, returning whether anything went.
    pub fn remove(&mut self, name: &str) -> bool {
        match self.position(name) {
            Some(index) => {
                self.items.remove(index);
                true
            }
            None => false,
        }
    }

    /// A name not already taken, derived from `base`.
    ///
    /// Used when adding a connection so two entries never collide, since the
    /// name is the key.
    pub fn unique_name(&self, base: &str) -> String {
        let base = if base.trim().is_empty() {
            "New connection"
        } else {
            base.trim()
        };
        if self.position(base).is_none() {
            return base.to_string();
        }
        for n in 2..=MAX_PROFILES + 1 {
            let candidate = format!("{base} {n}");
            if self.position(&candidate).is_none() {
                return candidate;
            }
        }
        // Only reachable with MAX_PROFILES entries already sharing the base.
        format!("{base} {}", self.items.len() + 1)
    }
}

/// Where [`Profiles::save`] stages the new file before renaming it into place.
///
/// The name carries this process's id, and that is the whole point. A fixed
/// `connections.toml.new` is shared by every launcher a user has open: two
/// of them saving at once write the same staging file, so one window's list
/// lands on top of the other's half-written bytes and the rename publishes
/// the mixture. A pid is enough to separate them -- a single process only
/// ever saves from its own UI thread -- and it keeps the leftover, if a save
/// dies outright, identifiable rather than anonymous.
fn temp_path(path: &Path) -> PathBuf {
    with_suffix(path, &format!(".{}.new", std::process::id()))
}

/// Write `bytes` to `path` and do not return until they are on the disk.
///
/// `fs::write` alone leaves the contents in the page cache. The rename that
/// follows is atomic with respect to *other processes*, but not with respect
/// to power loss: the directory entry can reach the disk before the data it
/// points at, so a crash in the wrong second leaves `connections.toml`
/// present, renamed, and empty -- every saved connection gone, with no
/// broken file to hint that anything was lost.
fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// `path` with `suffix` appended to the file name.
///
/// Built from the `OsString` rather than through `Display`, because a path
/// under a directory whose name is not valid UTF-8 would otherwise come back
/// with replacement characters in it and name a different file.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Reserve a name to move an unreadable connections file to.
///
/// `<path>.bad`, then `<path>.bad.2` and upwards. Numbered rather than
/// overwriting, because the file being moved aside is by definition the only
/// copy of connections we could not read, and a second bad save that
/// clobbered the first would destroy the thing the move exists to preserve.
///
/// The chosen name is created empty here rather than merely tested for
/// absence: two launcher windows can hit this at the same moment, and an
/// `exists()` check would let both pick the same name and one overwrite the
/// other. The empty file is replaced by the rename in [`move_aside`].
fn reserve_aside(path: &Path) -> Result<PathBuf> {
    for n in 1..=MAX_ASIDE {
        let candidate = match n {
            1 => with_suffix(path, ".bad"),
            n => with_suffix(path, &format!(".bad.{n}")),
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("creating {}", candidate.display())),
        }
    }
    bail!(
        "{} and {} numbered copies of it already exist",
        with_suffix(path, ".bad").display(),
        MAX_ASIDE - 1
    )
}

/// Move a connections file that could not be read out of the way, returning
/// where it went.
///
/// The escape hatch from an unparseable file: the launcher will not write
/// over one, so without this a single stray character in the file leaves the
/// user with a launcher that can never save again and no way out but a
/// terminal.
pub fn move_aside(path: &Path) -> Result<PathBuf> {
    let destination = reserve_aside(path)?;
    // Replaces the empty file reserved above; `fs::rename` overwrites on
    // both Unix and Windows.
    if let Err(e) = std::fs::rename(path, &destination) {
        let _ = std::fs::remove_file(&destination);
        return Err(e)
            .with_context(|| format!("moving {} to {}", path.display(), destination.display()));
    }
    Ok(destination)
}

/// Configuration directory for this application on this platform.
fn config_dir() -> Option<PathBuf> {
    // An explicit override wins everywhere, which is what the tests use and
    // what a portable install would want.
    if let Some(dir) = std::env::var_os("LYNXRDP_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("LynxRDP"),
        )
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA")?;
        Some(PathBuf::from(base).join("LynxRDP"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // XDG: $XDG_CONFIG_HOME, else ~/.config.
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return Some(PathBuf::from(xdg).join("lynxrdp"));
            }
        }
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".config").join("lynxrdp"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        let mut p = Profile::new("workstation");
        p.host = "10.0.0.5".into();
        p.user = "alice".into();
        p
    }

    // ---- Profile ------------------------------------------------------

    #[test]
    fn a_new_profile_has_the_conveniences_switched_on() {
        // Default would give false for both, which is the wrong starting
        // point for a form: these are opt-out features.
        let p = Profile::new("x");
        assert!(p.dynamic_resize);
        assert!(p.clipboard);
    }

    #[test]
    fn destination_joins_user_and_host() {
        assert_eq!(sample().destination(), "alice@10.0.0.5");
        let mut p = sample();
        p.user.clear();
        assert_eq!(p.destination(), "10.0.0.5");
    }

    #[test]
    fn a_plain_profile_produces_just_a_destination() {
        // Everything else is a default, so nothing else should be passed.
        assert_eq!(sample().args(), vec!["alice@10.0.0.5"]);
    }

    #[test]
    fn every_setting_reaches_the_command_line() {
        let mut p = sample();
        p.ssh_port = Some(2222);
        p.identity = Some(PathBuf::from("/k/id"));
        p.ssh_options = vec![
            "ProxyJump=bastion".into(),
            "StrictHostKeyChecking=yes".into(),
        ];
        p.remote_port = Some(3391);
        p.size = Some((1920, 1080));
        p.fullscreen = true;
        p.dynamic_resize = false;
        p.clipboard = false;
        let args = p.args();
        assert_eq!(args[0], "alice@10.0.0.5");
        assert!(args.windows(2).any(|w| w == ["--port", "2222"]));
        assert!(args.windows(2).any(|w| w == ["--identity", "/k/id"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--ssh-option", "ProxyJump=bastion"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--ssh-option", "StrictHostKeyChecking=yes"]));
        assert!(args.windows(2).any(|w| w == ["--remote-port", "3391"]));
        assert!(args.windows(2).any(|w| w == ["--size", "1920x1080"]));
        assert!(args.iter().any(|a| a == "--fullscreen"));
        assert!(args.iter().any(|a| a == "--no-dynamic-resize"));
        assert!(args.iter().any(|a| a == "--no-clipboard"));
    }

    #[test]
    fn the_opt_out_flags_are_absent_when_the_feature_is_on() {
        let p = sample();
        let args = p.args();
        assert!(!args.iter().any(|a| a == "--no-dynamic-resize"));
        assert!(!args.iter().any(|a| a == "--no-clipboard"));
        assert!(!args.iter().any(|a| a == "--fullscreen"));
    }

    #[test]
    fn arguments_are_separate_so_odd_values_cannot_split() {
        // Passed to the process directly, never through a shell, so a value
        // with a space stays one argument.
        let mut p = sample();
        p.identity = Some(PathBuf::from("/home/a b/id key"));
        let args = p.args();
        let index = args.iter().position(|a| a == "--identity").unwrap();
        assert_eq!(args[index + 1], "/home/a b/id key");
    }

    #[test]
    fn a_good_profile_has_no_problem() {
        assert_eq!(sample().problem(), None);
    }

    #[test]
    fn a_name_and_host_are_required() {
        let mut p = sample();
        p.name = "  ".into();
        assert!(p.problem().unwrap().contains("name"));

        let mut p = sample();
        p.host = "".into();
        assert!(p.problem().unwrap().contains("host"));
    }

    #[test]
    fn a_host_that_would_confuse_ssh_is_refused() {
        // These would become extra arguments or be split when they reach ssh.
        for host in ["two words", "-oProxyCommand=evil", "a\tb"] {
            let mut p = sample();
            p.host = host.into();
            assert!(p.problem().is_some(), "{host:?} should be refused");
        }
        let mut p = sample();
        p.user = "-oX=1".into();
        assert!(p.problem().is_some());
        let mut p = sample();
        p.user = "bob@elsewhere".into();
        assert!(p.problem().is_some());
    }

    #[test]
    fn zero_ports_and_sizes_are_refused() {
        let mut p = sample();
        p.ssh_port = Some(0);
        assert!(p.problem().is_some());
        let mut p = sample();
        p.remote_port = Some(0);
        assert!(p.problem().is_some());
        let mut p = sample();
        p.size = Some((0, 1080));
        assert!(p.problem().is_some());
    }

    #[test]
    fn ssh_options_must_look_like_options() {
        let mut p = sample();
        p.ssh_options = vec!["not an option".into()];
        assert!(p.problem().unwrap().contains("Key=value"));
        let mut p = sample();
        p.ssh_options = vec!["  ".into()];
        assert!(p.problem().unwrap().contains("blank"));
    }

    #[test]
    fn overlong_fields_are_refused() {
        let mut p = sample();
        p.name = "n".repeat(MAX_FIELD + 1);
        assert!(p.problem().is_some());
        let mut p = sample();
        p.host = "h".repeat(MAX_FIELD + 1);
        assert!(p.problem().is_some());
    }

    // ---- Profiles -----------------------------------------------------

    #[test]
    fn roundtrips_through_toml() {
        let mut store = Profiles::default();
        let mut p = sample();
        p.ssh_port = Some(2222);
        p.size = Some((1280, 720));
        p.ssh_options = vec!["ProxyJump=bastion".into()];
        store.upsert(p);
        store.upsert(Profile::new("empty"));
        let text = store.to_toml();
        assert_eq!(Profiles::from_toml(&text).unwrap(), store);
    }

    #[test]
    fn an_empty_file_is_an_empty_list() {
        assert_eq!(Profiles::from_toml("").unwrap(), Profiles::default());
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_dropped() {
        // Better to say the file is wrong than to quietly lose a setting.
        let text = "[[connection]]\nname = \"a\"\nhost = \"h\"\nbogus = 1\n";
        assert!(Profiles::from_toml(text).is_err());
    }

    #[test]
    fn an_absurd_number_of_connections_is_refused() {
        let mut text = String::new();
        for i in 0..(MAX_PROFILES + 1) {
            text.push_str(&format!("[[connection]]\nname = \"n{i}\"\nhost = \"h\"\n"));
        }
        assert!(Profiles::from_toml(&text).is_err());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope").join(FILE_NAME);
        assert_eq!(Profiles::load(&path).unwrap(), Profiles::default());
    }

    #[test]
    fn saving_creates_the_directory_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(FILE_NAME);
        let mut store = Profiles::default();
        store.upsert(sample());
        store.save(&path).unwrap();
        assert!(path.exists());
        assert_eq!(Profiles::load(&path).unwrap(), store);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        // The save is a write-then-rename so an interrupted save cannot
        // truncate the real file; the temporary must not linger either.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        Profiles::default().save(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != FILE_NAME)
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn the_staging_file_is_unique_to_this_process() {
        // A shared "connections.toml.new" is what lets two launcher windows
        // interleave their saves into one file.
        let path = Path::new("/cfg/connections.toml");
        let temporary = temp_path(path);
        assert_ne!(temporary, path.with_extension("toml.new"));
        assert!(temporary
            .to_string_lossy()
            .contains(&std::process::id().to_string()));
        assert_eq!(temporary.parent(), path.parent());
    }

    #[test]
    fn a_save_that_cannot_be_published_cleans_up_after_itself() {
        // A directory where the connections file should be: the staging
        // write succeeds and the rename cannot. The staging file is named
        // after this process, so leaving it would poison the next save.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let mut store = Profiles::default();
        store.upsert(sample());
        assert!(store.save(&path).is_err());
        assert!(
            !temp_path(&path).exists(),
            "the staging file was left behind"
        );
    }

    #[test]
    fn a_bad_file_is_moved_to_the_first_free_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);

        std::fs::write(&path, "not toml at all").unwrap();
        let first = move_aside(&path).unwrap();
        assert_eq!(first.file_name().unwrap(), "connections.toml.bad");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "not toml at all");
        assert!(!path.exists());

        // The second one must not overwrite the first: each is the only copy
        // of whatever connections it holds.
        std::fs::write(&path, "still not toml").unwrap();
        let second = move_aside(&path).unwrap();
        assert_eq!(second.file_name().unwrap(), "connections.toml.bad.2");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "not toml at all");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "still not toml");
    }

    #[test]
    fn a_hundred_bad_files_is_where_it_stops() {
        // The loop has to end somewhere, and the end has to be an error
        // rather than a silent reuse of the last name: reusing it would
        // overwrite a file that is, by definition, the only copy of some
        // connections we could not read.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        for _ in 0..MAX_ASIDE {
            reserve_aside(&path).unwrap();
        }
        let err = reserve_aside(&path).unwrap_err().to_string();
        assert!(err.contains("already exist"), "{err}");
    }

    #[test]
    fn reserving_an_aside_name_claims_it_rather_than_just_checking() {
        // Two launchers can reach this at the same moment; an exists() test
        // would let both pick the same name and one destroy the other.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let first = reserve_aside(&path).unwrap();
        let second = reserve_aside(&path).unwrap();
        assert_ne!(first, second);
        assert!(first.exists() && second.exists());
    }

    #[test]
    fn moving_a_missing_file_aside_leaves_no_empty_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert!(move_aside(&path).is_err());
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn a_name_in_use_is_reported_except_for_the_profile_being_edited() {
        let mut store = Profiles::default();
        store.upsert(Profile::new("work"));
        store.upsert(Profile::new("home"));

        // Adding: any existing name is taken.
        assert!(store.name_taken("work", None));
        assert!(!store.name_taken("lab", None));
        // Editing "work" and leaving the name alone is not a clash.
        assert!(!store.name_taken("work", Some("work")));
        // Renaming "work" onto "home" is: upsert would merge the two.
        assert!(store.name_taken("home", Some("work")));
        assert!(!store.name_taken("lab", Some("work")));
    }

    #[test]
    fn upsert_replaces_by_name_rather_than_duplicating() {
        let mut store = Profiles::default();
        assert!(!store.upsert(sample()));
        let mut changed = sample();
        changed.host = "10.0.0.9".into();
        assert!(store.upsert(changed));
        assert_eq!(store.items.len(), 1);
        assert_eq!(store.items[0].host, "10.0.0.9");
    }

    #[test]
    fn upsert_keeps_the_order_of_an_edited_entry() {
        // An edit must not make a row jump to the bottom of the list.
        let mut store = Profiles::default();
        store.upsert(Profile::new("a"));
        store.upsert(Profile::new("b"));
        store.upsert(Profile::new("c"));
        let mut edited = Profile::new("b");
        edited.host = "changed".into();
        store.upsert(edited);
        let names: Vec<_> = store.items.iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(store.items[1].host, "changed");
    }

    #[test]
    fn remove_reports_whether_it_did_anything() {
        let mut store = Profiles::default();
        store.upsert(sample());
        assert!(store.remove("workstation"));
        assert!(!store.remove("workstation"));
        assert!(store.items.is_empty());
    }

    #[test]
    fn unique_name_avoids_collisions() {
        let mut store = Profiles::default();
        assert_eq!(store.unique_name("New connection"), "New connection");
        store.upsert(Profile::new("New connection"));
        assert_eq!(store.unique_name("New connection"), "New connection 2");
        store.upsert(Profile::new("New connection 2"));
        assert_eq!(store.unique_name("New connection"), "New connection 3");
    }

    #[test]
    fn unique_name_handles_a_blank_base() {
        let store = Profiles::default();
        assert_eq!(store.unique_name("   "), "New connection");
    }

    #[test]
    fn unset_fields_are_left_out_of_the_file() {
        // The file is meant to be hand-editable, so a profile that sets
        // nothing optional should not produce a page of empty values.
        let mut store = Profiles::default();
        store.items.push(sample());
        let text = toml::to_string_pretty(&store).unwrap();
        for absent in [
            "ssh_port",
            "identity",
            "ssh_options",
            "remote_port",
            "size",
            "fullscreen",
        ] {
            // Matched at the start of a line: "size" is also a substring of
            // "dynamic_resize", which is written.
            let key = format!("\n{absent} =");
            assert!(!text.contains(&key), "{absent} should be omitted:\n{text}");
        }
        // The two opt-out flags must always be written: they default to
        // false, so leaving them out would quietly turn them off on reload.
        assert!(text.contains("dynamic_resize = true"), "{text}");
        assert!(text.contains("clipboard = true"), "{text}");
    }

    #[test]
    fn a_terse_file_reloads_to_the_same_profiles() {
        let mut store = Profiles::default();
        store.items.push(sample());
        let mut full = Profile::new("everything");
        full.host = "gpu-01.lan".into();
        full.user = "carol".into();
        full.ssh_port = Some(2200);
        full.identity = Some(PathBuf::from("/home/carol/.ssh/id_ed25519"));
        full.ssh_options = vec!["ProxyJump=bastion".into()];
        full.remote_port = Some(4000);
        full.size = Some((1600, 900));
        full.fullscreen = true;
        store.items.push(full);

        let text = toml::to_string_pretty(&store).unwrap();
        let back: Profiles = toml::from_str(&text).unwrap();
        assert_eq!(back.items, store.items);
    }

    #[test]
    fn the_config_directory_can_be_overridden() {
        // The override is what makes this testable without touching a real
        // home directory, and what a portable install would use.
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("LYNXRDP_CONFIG_DIR");
        std::env::set_var("LYNXRDP_CONFIG_DIR", dir.path());
        let path = Profiles::default_path().unwrap();
        match previous {
            Some(v) => std::env::set_var("LYNXRDP_CONFIG_DIR", v),
            None => std::env::remove_var("LYNXRDP_CONFIG_DIR"),
        }
        assert_eq!(path, dir.path().join(FILE_NAME));
    }
}
