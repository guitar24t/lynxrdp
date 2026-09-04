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
//! were malformed would be absurd. So this file is parsed leniently, and load
//! failures go to the status line only.
//!
//! Nothing secret lives here either. These are window preferences; SSH still
//! owns authentication.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            compact_rows: false,
            show_command_line: false,
            zoom: 1.0,
        }
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
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let mut parsed: Self =
                    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
                // A hand-edited or truncated zoom must not leave the window at
                // 0.01. Clamped rather than rejected: the rest of the file is
                // still worth having.
                parsed.zoom = clamp_zoom(parsed.zoom);
                Ok(parsed)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write the file, creating the directory if needed.
    ///
    /// A plain write, not the staged rename [`crate::profiles::Profiles::save`]
    /// uses. That dance exists because a half-written connections file loses
    /// hosts the user typed by hand; a half-written preferences file simply
    /// fails to parse on the next start and falls back to the defaults, which
    /// is the behaviour this module is built around anyway.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising settings")?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }
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
        // The caller shows this on the status line and carries on with the
        // defaults; it must not be mistaken for the connections file's
        // read-only state.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        std::fs::write(&path, "this is not toml").unwrap();
        assert!(Settings::load(&path).is_err());
    }

    #[test]
    fn the_settings_file_sits_beside_the_connections_file() {
        let path = Settings::path_beside(Path::new("/cfg/connections.toml"));
        assert_eq!(path, PathBuf::from("/cfg/settings.toml"));
    }
}
