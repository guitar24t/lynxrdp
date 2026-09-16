//! View preferences for the launcher: theme, row density, zoom.
//!
//! A second file, `settings.toml`, beside `connections.toml` rather than a
//! table inside it. Two reasons, and both are load-bearing.
//!
//! [`crate::profiles::Profiles`] is `deny_unknown_fields`, so a `[view]` table
//! added to `connections.toml` would make **every older client refuse to parse
//! the file** -- and refusing to parse it is exactly what puts the launcher
//! into its read-only state, with saving disabled and the list blank. A theme
//! preference is not worth that.
//!
//! The two files also want opposite failure behaviour. An unreadable
//! `connections.toml` must stop writes, because the file holds data that
//! cannot be reconstructed. An unreadable `settings.toml` should fall back to
//! the defaults and be rewritten by the next change: a lost theme choice is a
//! shrug, and a launcher that refused to work because its window preferences
//! were malformed would be absurd. So this file is parsed leniently -- one
//! field at a time, so a value that cannot be read costs that value and not
//! the rest -- and load failures go to the status line only.
//!
//! The one preference that is not a shrug is `updates.check`, the only thing
//! in here that reaches the network. SECURITY.md promises that turning it
//! off means nothing is sent, and a typo in `theme` must not be what breaks
//! that promise: a file that cannot be read is treated as having the check
//! *off*, keeping whatever the file still says about it plainly, rather than
//! as the defaults, whose check is on.
//!
//! Nothing secret lives here either. These are window preferences; SSH still
//! owns authentication.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// File name inside the configuration directory.
pub const FILE_NAME: &str = "settings.toml";

/// Which theme the user picked.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Follow the desktop where that can be read. It cannot be read on X11 --
    /// winit reports no theme there and egui falls back to dark -- which is
    /// why the menu item says so rather than promising something it cannot
    /// deliver.
    #[default]
    System,
    Light,
    Dark,
}

impl From<ThemeChoice> for eframe::egui::ThemePreference {
    fn from(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::System => Self::System,
            ThemeChoice::Light => Self::Light,
            ThemeChoice::Dark => Self::Dark,
        }
    }
}

/// What the updater is allowed to do.
///
/// A separate table so the settings file says plainly what it is: the one
/// preference here that reaches the network is worth being able to find and
/// turn off with an editor, on a machine where the window is not convenient.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Updates {
    /// Ask GitHub for the release list, once a day at most.
    pub check: bool,
    /// Offer release candidates.
    ///
    /// `None` -- the default, and an absent key in the file -- means "the
    /// same kind of build I am running". Every release so far is a
    /// candidate, so a plain `false` would tell everyone who has one that
    /// they are up to date forever; and once `v0.1.0` proper exists, someone
    /// who installed it should not be moved onto the next candidate series
    /// without having asked. `Some` is an explicit choice and is obeyed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prereleases: Option<bool>,
    /// When the last check happened, in seconds since the epoch, so that
    /// starting the launcher five times in a morning is still one request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_check: Option<u64>,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            // On, because a remote desktop client that silently stays on an
            // old build is the worse failure -- and the check only ever
            // *offers*: nothing is downloaded or replaced without a click.
            check: true,
            prereleases: None,
            last_check: None,
        }
    }
}

/// Everything the View menu remembers.
///
/// Not `deny_unknown_fields`, unlike the connections file: a preference
/// written by a newer client must be ignored by an older one, not turn the
/// whole file into an error that throws away the preferences it *could* read.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// One-line rows instead of two.
    pub compact_rows: bool,
    /// Show the command line the selected connection would run.
    pub show_command_line: bool,
    /// egui zoom factor, the low-vision path.
    pub zoom: f32,
    /// Update checking. Last, because it is a table: TOML requires every
    /// plain value to be written before the first one.
    pub updates: Updates,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            compact_rows: false,
            show_command_line: false,
            zoom: 1.0,
            updates: Updates::default(),
        }
    }
}

/// A settings file that could not be read in full.
///
/// Carries the settings to use as well as the reason, because the caller
/// wants both: the reason for the status line, and the settings -- every
/// value the file still said plainly, and above all the update opt-out -- to
/// run with. A plain error would leave it reaching for `Settings::default()`,
/// whose `updates.check` is on.
#[derive(Debug)]
pub struct LoadError {
    pub salvaged: Settings,
    pub cause: anyhow::Error,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.cause)
    }
}

impl Settings {
    /// Where the settings file sits, given the connections file.
    ///
    /// Derived from the connections path rather than looked up separately, so
    /// that `LYNXRDP_CONFIG_DIR` and a test's temporary directory move both
    /// files together.
    pub fn path_beside(connections: &Path) -> PathBuf {
        match connections.parent() {
            Some(dir) => dir.join(FILE_NAME),
            None => PathBuf::from(FILE_NAME),
        }
    }

    /// Read the file. A missing one is the defaults, not an error.
    ///
    /// An `Err` still carries settings to run with; see [`LoadError`].
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| LoadError {
                salvaged: e.salvaged,
                cause: e.cause.context(format!("reading {}", path.display())),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(LoadError {
                salvaged: Self::unreadable(),
                cause: anyhow::Error::from(e).context(format!("reading {}", path.display())),
            }),
        }
    }

    /// Parse the file's text, one field at a time.
    ///
    /// Into a table first and then field by field, rather than straight into
    /// the struct, because serde's whole-struct deserialisation is all or
    /// nothing: `theme = "blue"` would take the zoom, the row density and
    /// the update opt-out down with it. Here a value that cannot be read is
    /// left at its default and named in the error, and every other value is
    /// kept. Unknown keys are ignored, as before -- see the note on
    /// [`Settings`].
    ///
    /// A file that is not TOML at all cannot be read field by field. That
    /// case is [`Self::unreadable`] plus whatever [`salvage_check`] can
    /// still make out, so that a truncated or mistyped file never turns a
    /// chosen `check = false` into a request to github.com.
    pub fn parse(text: &str) -> Result<Self, LoadError> {
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(e) => {
                let mut salvaged = Self::unreadable();
                if let Some(check) = salvage_check(text) {
                    salvaged.updates.check = check;
                }
                return Err(LoadError {
                    salvaged,
                    cause: anyhow::Error::from(e).context("parsing settings"),
                });
            }
        };
        let mut settings = Self::default();
        let mut problems = Vec::new();
        set(
            &mut settings.theme,
            field(&table, "theme", ""),
            &mut problems,
        );
        set(
            &mut settings.compact_rows,
            field(&table, "compact_rows", ""),
            &mut problems,
        );
        set(
            &mut settings.show_command_line,
            field(&table, "show_command_line", ""),
            &mut problems,
        );
        set(&mut settings.zoom, field(&table, "zoom", ""), &mut problems);
        // A hand-edited or truncated zoom must not leave the window at 0.01.
        // Clamped rather than rejected: the rest of the file is still worth
        // having.
        settings.zoom = clamp_zoom(settings.zoom);
        match table.get("updates") {
            None => {}
            Some(toml::Value::Table(updates)) => {
                let check = field(updates, "check", "updates.");
                if check.is_err() {
                    // Present and unreadable is not the same as absent:
                    // somebody wrote something here, and until it is fixed
                    // the only reading that keeps the promise is "off".
                    settings.updates.check = false;
                }
                set(&mut settings.updates.check, check, &mut problems);
                set(
                    &mut settings.updates.prereleases,
                    field(updates, "prereleases", "updates.").map(|v| v.map(Some)),
                    &mut problems,
                );
                set(
                    &mut settings.updates.last_check,
                    field(updates, "last_check", "updates.").map(|v| v.map(Some)),
                    &mut problems,
                );
            }
            Some(other) => {
                settings.updates.check = false;
                problems.push(format!(
                    "updates: expected a table, found {}",
                    other.type_str()
                ));
            }
        }
        if problems.is_empty() {
            Ok(settings)
        } else {
            Err(LoadError {
                salvaged: settings,
                cause: anyhow!("{}", problems.join("; ")),
            })
        }
    }

    /// What to run with when the file cannot be read at all.
    ///
    /// The defaults, except that the update check is off: the file may well
    /// have said so, and a request to github.com the user had turned off is
    /// the one thing a broken preferences file must not cause. The next
    /// change the user makes writes the file afresh, and the choice is theirs
    /// to remake from the Help menu.
    fn unreadable() -> Self {
        Self {
            updates: Updates {
                check: false,
                ..Updates::default()
            },
            ..Self::default()
        }
    }

    /// Write the file, creating the directory if needed.
    ///
    /// Staged and renamed like the connections file, and for a reason of the
    /// same shape at a smaller scale: `fs::write` truncates before it writes,
    /// so a launcher killed in between leaves an *empty* file -- which is
    /// valid TOML, reads as every default, and would quietly turn the update
    /// check back on with nothing on the status line to say so. The theme and
    /// the zoom could bear that; the one preference that reaches the network
    /// cannot.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising settings")?;
        let temporary = crate::profiles::temp_path(path);
        if let Err(e) = crate::profiles::write_durably(&temporary, text.as_bytes()) {
            let _ = std::fs::remove_file(&temporary);
            return Err(e).with_context(|| format!("writing {}", temporary.display()));
        }
        if let Err(e) = std::fs::rename(&temporary, path) {
            let _ = std::fs::remove_file(&temporary);
            return Err(e).with_context(|| format!("replacing {}", path.display()));
        }
        Ok(())
    }
}

/// One value out of `table`: `Ok(None)` when the key is absent, `Err` when
/// it is present but not what was expected.
///
/// Absent is not a problem -- an older file simply has fewer keys -- but a
/// value that is there and unreadable is worth a line, since somebody typed
/// it. `prefix` names the table for that line.
fn field<T: DeserializeOwned>(
    table: &toml::Table,
    key: &str,
    prefix: &str,
) -> Result<Option<T>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .clone()
            .try_into()
            .map(Some)
            .map_err(|e| format!("{prefix}{key}: {}", e.message())),
    }
}

/// Put a read value where it goes, or record why it could not be read.
fn set<T>(slot: &mut T, found: Result<Option<T>, String>, problems: &mut Vec<String>) {
    match found {
        Ok(Some(value)) => *slot = value,
        Ok(None) => {}
        Err(problem) => problems.push(problem),
    }
}

/// What a file that is not valid TOML still says about `updates.check`.
///
/// Line by line, trusting only a `check = true` or `check = false` under an
/// `[updates]` heading and spelled plainly -- with a capital allowed, because
/// `check = False` is exactly the slip that makes the whole file unparseable.
/// Anything less certain is `None`, which the caller reads as off.
fn salvage_check(text: &str) -> Option<bool> {
    let mut in_updates = false;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_updates = line == "[updates]";
            continue;
        }
        if !in_updates {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "check" {
            continue;
        }
        let value = value.trim();
        if value.eq_ignore_ascii_case("true") {
            return Some(true);
        }
        if value.eq_ignore_ascii_case("false") {
            return Some(false);
        }
        return None;
    }
    None
}

/// Keep zoom inside the range the window was designed for.
pub fn clamp_zoom(zoom: f32) -> f32 {
    if zoom.is_finite() {
        zoom.clamp(crate::theme::ZOOM_MIN, crate::theme::ZOOM_MAX)
    } else {
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(settings: &Settings) -> Settings {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        settings.save(&path).unwrap();
        Settings::load(&path).unwrap()
    }

    #[test]
    fn preferences_survive_a_restart() {
        let settings = Settings {
            theme: ThemeChoice::Light,
            compact_rows: true,
            show_command_line: true,
            zoom: 1.5,
            updates: Updates {
                check: false,
                prereleases: Some(true),
                last_check: Some(1_757_000_000),
            },
        };
        assert_eq!(roundtrip(&settings), settings);
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nowhere").join(FILE_NAME);
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
    }

    #[test]
    fn a_key_from_a_newer_client_is_ignored_rather_than_fatal() {
        // The opposite of the connections file on purpose: losing every
        // preference because one of them is from a later version would be a
        // worse outcome than ignoring the one we do not understand.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "theme = \"dark\"\nsomething_new = 3\n").unwrap();
        assert_eq!(Settings::load(&path).unwrap().theme, ThemeChoice::Dark);
    }

    #[test]
    fn an_absurd_zoom_is_clamped_rather_than_obeyed() {
        // A hand-edited or truncated value must not leave a window the user
        // cannot read well enough to fix it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "zoom = 0.01\n").unwrap();
        assert_eq!(Settings::load(&path).unwrap().zoom, crate::theme::ZOOM_MIN);
        std::fs::write(&path, "zoom = 50.0\n").unwrap();
        assert_eq!(Settings::load(&path).unwrap().zoom, crate::theme::ZOOM_MAX);
        assert_eq!(clamp_zoom(f32::NAN), 1.0);
    }

    #[test]
    fn an_unparseable_file_is_reported_rather_than_silently_reset() {
        // The caller shows this on the status line and carries on with what
        // was salvaged; it must not be mistaken for the connections file's
        // read-only state.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "this is not toml").unwrap();
        let err = Settings::load(&path).unwrap_err();
        assert!(err.to_string().contains("parsing settings"), "{err}");
        // With nothing to go on, the one preference that reaches the network
        // is off: the file may well have said so.
        assert!(!err.salvaged.updates.check);
        assert_eq!(err.salvaged.theme, ThemeChoice::System);
    }

    #[test]
    fn a_bad_value_costs_only_that_value() {
        // The reason for parsing field by field: `theme = "blue"` used to
        // fail the whole file, and the defaults it fell back to had the
        // update check on -- which the first frame then saved over the
        // user's `check = false`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(
            &path,
            "theme = \"blue\"\nzoom = 1.5\ncompact_rows = true\n\n[updates]\ncheck = false\nlast_check = 5\n",
        )
        .unwrap();
        let err = Settings::load(&path).unwrap_err();
        assert!(err.to_string().contains("theme"), "{err}");
        let got = err.salvaged;
        assert_eq!(got.theme, ThemeChoice::System, "the bad value is defaulted");
        assert_eq!(got.zoom, 1.5);
        assert!(got.compact_rows);
        assert!(!got.updates.check, "the opt-out survived the typo");
        assert_eq!(got.updates.last_check, Some(5));
    }

    #[test]
    fn the_update_opt_out_survives_a_file_that_is_not_toml() {
        // A capital False, and a line truncated by a crash, are each enough
        // to make the whole file unparseable. What it still says plainly
        // about the check is kept; what it does not say is read as off.
        let salvaged = |text: &str| Settings::parse(text).unwrap_err().salvaged.updates.check;
        assert!(!salvaged("theme = \"dark\"\n[updates]\ncheck = False\n"));
        assert!(!salvaged("[updates]\ncheck = false\nlast_ch"));
        assert!(salvaged(
            "zoom = 1.2\n[updates]\ncheck = true\nprereleases = ye"
        ));
        // A `check` under some other table is not the one that matters.
        assert!(!salvaged(
            "[other]\ncheck = true\n[updates]\nlast_check = x\n"
        ));
        assert!(!salvaged("check = true\n[updates]\nlast_check = x\n"));
        assert!(!salvaged("garbage"));
    }

    #[test]
    fn a_present_but_unreadable_check_reads_as_off() {
        // Unlike an absent key, somebody wrote this; until they fix it the
        // only reading that keeps SECURITY.md's promise is the quiet one.
        let err = Settings::parse("[updates]\ncheck = \"yes\"\n").unwrap_err();
        assert!(err.to_string().contains("updates.check"), "{err}");
        assert!(!err.salvaged.updates.check);
        let err = Settings::parse("updates = 3\n").unwrap_err();
        assert!(!err.salvaged.updates.check);
        // And an absent one is the default, which is on.
        assert!(Settings::parse("theme = \"dark\"\n").unwrap().updates.check);
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        // Staged and renamed, so an interrupted save cannot leave an empty
        // file that reads as every default; the staging file must not linger
        // either.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        Settings::default().save(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != FILE_NAME)
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn the_settings_file_sits_beside_the_connections_file() {
        let path = Settings::path_beside(Path::new("/cfg/connections.toml"));
        assert_eq!(path, PathBuf::from("/cfg/settings.toml"));
    }
}
