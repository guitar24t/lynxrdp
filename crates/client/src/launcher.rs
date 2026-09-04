//! The connection manager window.
//!
//! What `lynxrdp` shows when it is started with no arguments: a list of saved
//! connections, and an editor for them. Connecting spawns a session as a
//! child process (see [`crate::launch`]) and leaves this window open, so
//! several sessions can run at once.
//!
//! The window owns no protocol logic at all. It edits [`Profile`] values,
//! asks them whether they are valid, and turns the chosen one into a command
//! line. That keeps everything worth testing in `profiles` and `launch`,
//! where it can be tested without a display.
//!
//! Every menu item and every accelerator goes through one [`Action`] and one
//! [`Launcher::perform`]. Two code paths reaching the same feature is how a
//! menu ends up doing something subtly different from the button beside it,
//! and it is also what makes almost all of this testable: `perform` needs a
//! bare `egui::Context`, never a window.
//!
//! Colours, type and spacing come from [`crate::theme`], which the session
//! window shares. Nothing here names a colour.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eframe::egui;

use crate::launch::Sessions;
use crate::profiles::{self, Profile, Profiles};
use crate::settings::{Settings, ThemeChoice};
use crate::theme;

/// Where "Documentation" goes.
const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");

/// Default file name for both halves of import and export.
///
/// One name, because the two are the same operation seen from either end:
/// export here, copy the file, import there.
const EXCHANGE_FILE: &str = "connections-export.toml";

/// Open the launcher, returning when the window is closed.
pub fn run(path: PathBuf) -> Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("LynxRDP")
        // Matches StartupWMClass in the .desktop entry, which is how a Linux
        // desktop attaches a running window to its launcher icon.
        .with_app_id(crate::APP_ID)
        .with_inner_size([880.0, 560.0])
        .with_min_inner_size([640.0, 420.0]);
    if let Some(icon) = crate::icon::load() {
        viewport = viewport.with_icon(egui::IconData {
            rgba: icon.rgba,
            width: icon.width,
            height: icon.height,
        });
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "LynxRDP",
        options,
        Box::new(move |cc| {
            let launcher = Launcher::new(path);
            launcher.install(&cc.egui_ctx);
            Ok(Box::new(launcher))
        }),
    )
    .map_err(|e| anyhow::anyhow!("could not open the launcher window: {e}"))
}

/// The accelerators, in one place so the menu can print exactly what the key
/// handler consumes.
///
/// `Modifiers::COMMAND` rather than CTRL: egui resolves it to ⌘ on macOS and
/// Ctrl everywhere else, in the matching *and* in `format_shortcut`, so one
/// declaration gives the right key and the right label on every platform.
mod keys {
    use eframe::egui::{Key, KeyboardShortcut, Modifiers};

    const CMD: Modifiers = Modifiers::COMMAND;
    const CMD_SHIFT: Modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);
    const PLAIN: Modifiers = Modifiers::NONE;

    pub const NEW: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::N);
    pub const DUPLICATE: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::D);
    // The key a Mac keyboard prints "delete" on is Backspace; Delete is the
    // forward-delete that only a full-size board has. The menu shows whichever
    // one this platform's users actually have, and the launcher answers to
    // both, because a wrong guess here is a feature nobody can reach.
    #[cfg(target_os = "macos")]
    pub const DELETE: KeyboardShortcut = KeyboardShortcut::new(PLAIN, Key::Backspace);
    #[cfg(not(target_os = "macos"))]
    pub const DELETE: KeyboardShortcut = KeyboardShortcut::new(PLAIN, Key::Delete);
    #[cfg(target_os = "macos")]
    pub const DELETE_ALT: KeyboardShortcut = KeyboardShortcut::new(PLAIN, Key::Delete);
    #[cfg(not(target_os = "macos"))]
    pub const DELETE_ALT: KeyboardShortcut = KeyboardShortcut::new(PLAIN, Key::Backspace);
    pub const IMPORT: KeyboardShortcut = KeyboardShortcut::new(CMD_SHIFT, Key::I);
    pub const EXPORT: KeyboardShortcut = KeyboardShortcut::new(CMD_SHIFT, Key::E);
    pub const QUIT: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::Q);

    pub const CONNECT: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::Enter);
    pub const EDIT: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::E);
    pub const COPY_COMMAND: KeyboardShortcut = KeyboardShortcut::new(CMD_SHIFT, Key::C);
    pub const RELOAD: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::R);

    pub const ZOOM_IN: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::Equals);
    pub const ZOOM_OUT: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::Minus);
    pub const ZOOM_RESET: KeyboardShortcut = KeyboardShortcut::new(CMD, Key::Num0);

    pub const SHORTCUTS: KeyboardShortcut = KeyboardShortcut::new(PLAIN, Key::F1);

    /// Not a menu item: `/` jumps to the filter box, the way it does in every
    /// list a sysadmin already uses.
    pub const FILTER: KeyboardShortcut = KeyboardShortcut::new(PLAIN, Key::Slash);
}

/// Id of the filter field.
///
/// Fixed rather than generated because the key handler runs *before* the
/// widget exists each frame and has to know whether the user is typing into
/// it: `Delete` deletes a connection unless it is deleting a character.
const FILTER_ID: &str = "lynxrdp-filter";

/// One thing the launcher can be asked to do.
///
/// Menu items, accelerators and the buttons on the action bar all produce
/// these rather than acting directly, so there is one implementation of each
/// feature and one place to test it.
#[derive(Clone, Debug, PartialEq)]
enum Action {
    New,
    Duplicate,
    AskDelete,
    AskImport,
    AskExport,
    Quit,

    Connect,
    Edit,
    CopyCommandLine,
    CopyConnectionsPath,
    Reload,
    MoveAside,

    SetTheme(ThemeChoice),
    ToggleCompactRows,
    ToggleCommandLine,
    Zoom(f32),

    ShowShortcuts,
    ShowAbout,
    OpenDocumentation,

    /// Move the selection by n rows through what the filter is showing.
    Move(isize),
    FocusFilter,
    ClearFilter,
}

/// Which screen is showing.
enum View {
    List,
    // Boxed: the editor is much larger than the other variant, and every
    // View would otherwise be that size.
    Edit(Box<Editor>),
}

/// A modal that is up.
///
/// Import and export keep their typed path here rather than in the launcher,
/// so dismissing one throws the half-typed path away instead of leaving it to
/// reappear the next time the dialog is opened.
enum Dialog {
    ConfirmDelete(String),
    Import(String),
    Export(String),
    Shortcuts,
    About,
}

/// A profile being edited.
///
/// Numbers and the screen size are held as text while the user types: parsing
/// on every keystroke would fight them, for instance by erasing a half-typed
/// "19" on the way to "1920".
struct Editor {
    /// Name this started as, so a rename can remove the old entry. `None`
    /// when adding.
    original: Option<String>,
    profile: Profile,
    ssh_port: String,
    remote_port: String,
    identity: String,
    size: String,
    ssh_options: String,
    /// The last refusal, shown under the form as well as on the status line.
    /// A message about the field you are looking at should not be at the far
    /// edge of the window.
    problem: Option<String>,
}

impl Editor {
    fn new(profile: Profile, original: Option<String>) -> Self {
        Self {
            ssh_port: profile.ssh_port.map(|p| p.to_string()).unwrap_or_default(),
            remote_port: profile
                .remote_port
                .map(|p| p.to_string())
                .unwrap_or_default(),
            identity: profile
                .identity
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            size: profile
                .size
                .map(|(w, h)| format!("{w}x{h}"))
                .unwrap_or_default(),
            ssh_options: profile.ssh_options.join("\n"),
            profile,
            original,
            problem: None,
        }
    }

    /// Fold the text fields back into the profile, or say what is wrong.
    fn collect(&self) -> Result<Profile, String> {
        let mut p = self.profile.clone();
        p.name = p.name.trim().to_string();
        p.host = p.host.trim().to_string();
        p.user = p.user.trim().to_string();
        p.ssh_port = parse_port(&self.ssh_port, "SSH port")?;
        p.remote_port = parse_port(&self.remote_port, "LynxRDP port")?;
        p.identity = match self.identity.trim() {
            "" => None,
            text => Some(PathBuf::from(text)),
        };
        p.size = parse_size(&self.size)?;
        p.ssh_options = self
            .ssh_options
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if let Some(problem) = p.problem() {
            return Err(problem);
        }
        Ok(p)
    }
}

/// An empty field means "unset", which is different from a bad one.
fn parse_port(text: &str, what: &str) -> Result<Option<u16>, String> {
    match text.trim() {
        "" => Ok(None),
        t => match t.parse::<u16>() {
            Ok(0) | Err(_) => Err(format!("{what} must be a number between 1 and 65535.")),
            Ok(p) => Ok(Some(p)),
        },
    }
}

/// Parse `1920x1080`, accepting a capital X and spaces around the separator.
fn parse_size(text: &str) -> Result<Option<(u16, u16)>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let bad = || "The size should look like 1920x1080.".to_string();
    let (w, h) = text.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u16 = w.trim().parse().map_err(|_| bad())?;
    let h: u16 = h.trim().parse().map_err(|_| bad())?;
    if w == 0 || h == 0 {
        return Err("The size must be positive.".into());
    }
    Ok(Some((w, h)))
}

/// A profile's arguments as a line a user could paste into a shell.
///
/// Display only. The launcher spawns the binary with [`Profile::args`] as
/// separate arguments and must keep doing so -- building a command *string*
/// and handing it to a shell is how a host with a quote in it turns into
/// extra arguments. This exists so the window can show that the GUI and the
/// command line are the same thing, which is the whole reason sessions are
/// started by re-invoking this binary.
///
/// Quoting is POSIX; on Windows the line is illustrative rather than
/// paste-ready, and nothing acts on it either way.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty()
                || arg
                    .chars()
                    .any(|c| c.is_whitespace() || "'\"$\\`*?[]&|;<>()#~!".contains(c))
            {
                format!("'{}'", arg.replace('\'', r"'\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether a profile matches what is typed in the filter box.
///
/// Case-insensitive, over the name and the address, because those are the two
/// things a user remembers a host by. The address covers the user and the
/// host separately as well, both being substrings of `user@host`.
fn matches_filter(profile: &Profile, needle: &str) -> bool {
    profile.name.to_lowercase().contains(needle)
        || profile.destination().to_lowercase().contains(needle)
}

/// The application state.
struct Launcher {
    path: PathBuf,
    settings_path: PathBuf,
    store: Profiles,
    settings: Settings,
    view: View,
    dialog: Option<Dialog>,
    selected: Option<usize>,
    sessions: Sessions,
    /// What the list is filtered down to. Not persisted: a filter is a thing
    /// you are doing now, not a preference.
    filter: String,
    /// Put the caret in the filter box on the next frame.
    focus_filter: bool,
    /// Bring the selected row into view on the next frame, after a keyboard
    /// move that may have gone past the bottom of the list.
    scroll_to_selection: bool,
    /// Transient line under the list: what just happened.
    status: String,
    /// A problem worth showing in red until it is dealt with.
    error: Option<String>,
    /// Name awaiting a second click to confirm deletion.
    confirming_delete: Option<String>,
    /// The connections file exists but could not be parsed.
    ///
    /// Carried as state rather than left as a warning in `error`, because it
    /// has to *stop* things rather than describe them. The file holds
    /// connections we could not read; the list is therefore empty; and a
    /// save would publish that emptiness over the real thing. The user has
    /// to fix the file or move it aside before anything writes it again.
    load_failed: bool,
}

impl Launcher {
    fn new(path: PathBuf) -> Self {
        let (store, error, load_failed) = match Profiles::load(&path) {
            Ok(store) => (store, None, false),
            // A broken file must not leave a blank window with no
            // explanation, and must not be silently overwritten either.
            Err(e) => (
                Profiles::default(),
                Some(format!(
                    "Could not read {}: {e:#}. Fix it, or move it aside, before adding connections.",
                    path.display()
                )),
                true,
            ),
        };
        let selected = if store.items.is_empty() {
            None
        } else {
            Some(0)
        };
        let settings_path = Settings::path_beside(&path);
        // A settings file we cannot read is a shrug, not a state: the
        // defaults are perfectly usable and the next change rewrites it. It
        // must never reach `load_failed`, which exists to protect data.
        let (settings, status) = match Settings::load(&settings_path) {
            Ok(settings) => (settings, String::new()),
            Err(e) => (
                Settings::default(),
                format!("Using the default view settings: {e:#}"),
            ),
        };
        Self {
            path,
            settings_path,
            store,
            settings,
            view: View::List,
            dialog: None,
            selected,
            sessions: Sessions::default(),
            filter: String::new(),
            focus_filter: false,
            scroll_to_selection: false,
            status,
            error,
            confirming_delete: None,
            load_failed,
        }
    }

    /// Put the palette and the saved view preferences on a context.
    ///
    /// Once, at startup. `apply` installs a style for *both* themes, so the
    /// theme preference can stay on `System` and still get our colours.
    fn install(&self, ctx: &egui::Context) {
        theme::apply(ctx);
        ctx.set_theme(egui::ThemePreference::from(self.settings.theme));
        ctx.set_zoom_factor(self.settings.zoom);
    }

    fn save(&mut self) {
        if self.load_failed {
            // A backstop, not the gate. `save_editor` has already set
            // "Saved {name}" by the time it reaches here, so a refusal that
            // lived only in this function would still tell the user their
            // connection had been saved while the file went untouched. The
            // gate that matters is the disabled New button.
            self.status.clear();
            self.error = Some(format!(
                "Not saving: {} could not be read, and writing now would replace whatever is still in it.",
                self.path.display()
            ));
            return;
        }
        if let Err(e) = self.store.save(&self.path) {
            self.error = Some(format!("Could not save connections: {e:#}"));
        }
    }

    /// Persist the view preferences. A failure here is worth a line, not a
    /// state: the window still works, the choice just will not survive.
    fn save_settings(&mut self) {
        if let Err(e) = self.settings.save(&self.settings_path) {
            self.error = Some(format!("Could not save the view settings: {e:#}"));
        }
    }

    /// Rename the unreadable file out of the way and carry on with an empty
    /// list.
    ///
    /// The way out of [`Self::load_failed`] that does not involve a
    /// terminal. It keeps the file rather than deleting it: the user may
    /// well want to pick their hosts back out of it by hand.
    fn move_bad_file_aside(&mut self) {
        match profiles::move_aside(&self.path) {
            Ok(destination) => {
                self.store = Profiles::default();
                self.selected = None;
                self.load_failed = false;
                self.error = None;
                self.status = format!("Moved the unreadable file to {}", destination.display());
            }
            Err(e) => self.error = Some(format!("Could not move {}: {e:#}", self.path.display())),
        }
    }

    /// Re-read the connections file, keeping the selection on the same name.
    ///
    /// Worth a menu item because the README tells people to hand-edit the
    /// file, and because it is the way back out of `load_failed` once the
    /// file has been repaired in an editor.
    fn reload(&mut self) {
        let previous = self.selected_name();
        match Profiles::load(&self.path) {
            Ok(store) => {
                let count = store.items.len();
                self.store = store;
                self.load_failed = false;
                self.error = None;
                self.confirming_delete = None;
                self.selected = previous.and_then(|name| self.store.position(&name)).or(
                    if self.store.items.is_empty() {
                        None
                    } else {
                        Some(0)
                    },
                );
                self.status = match count {
                    1 => "Reloaded 1 connection".to_string(),
                    n => format!("Reloaded {n} connections"),
                };
            }
            Err(e) => {
                self.store = Profiles::default();
                self.selected = None;
                self.load_failed = true;
                self.status.clear();
                self.error = Some(format!(
                    "Could not read {}: {e:#}. Fix it, or move it aside, before adding connections.",
                    self.path.display()
                ));
            }
        }
    }

    /// Show anything the sessions that just exited had to say.
    ///
    /// Separate from `update` so it can be tested without a window, and it
    /// drains a queue rather than reading a return value because
    /// [`Sessions::reap`] runs more than once per repaint -- see its own
    /// comment.
    fn drain_failures(&mut self) {
        let mut messages = Vec::new();
        while let Some(failure) = self.sessions.take_failure() {
            messages.push(failure.message());
        }
        let Some(newest) = messages.pop() else { return };
        self.status.clear();
        // The others are counted rather than shown. Several sessions failing
        // in the same instant is the ordinary case, not a strange one -- an
        // SSH agent that went away takes every reconnection with it, and
        // they all say the same thing -- but assigning to `error` once per
        // failure would drop all but the last without a trace, and stacking
        // the full text of each would fill the window.
        self.error = Some(match messages.len() {
            0 => newest,
            1 => format!("{newest}\n(one other session failed as well)"),
            n => format!("{newest}\n({n} other sessions failed as well)"),
        });
    }

    fn connect(&mut self, index: usize) {
        let Some(profile) = self.store.items.get(index).cloned() else {
            return;
        };
        if let Some(problem) = profile.problem() {
            self.error = Some(format!("{} cannot be used: {problem}", profile.name));
            return;
        }
        match self.sessions.start(&profile) {
            Ok(()) => {
                self.status = format!("Connecting to {}", profile.destination());
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not start the session: {e:#}")),
        }
    }

    // ---- selection -----------------------------------------------------

    /// The indices the filter is letting through, in list order.
    fn visible(&self) -> Vec<usize> {
        let needle = self.filter.trim().to_lowercase();
        self.store
            .items
            .iter()
            .enumerate()
            .filter(|(_, p)| needle.is_empty() || matches_filter(p, &needle))
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_profile(&self) -> Option<&Profile> {
        self.selected.and_then(|i| self.store.items.get(i))
    }

    fn selected_name(&self) -> Option<String> {
        self.selected_profile().map(|p| p.name.clone())
    }

    /// Whether the selection is one of the rows the filter is showing.
    ///
    /// Filtering does not clear the selection -- typing four characters
    /// should not lose the host you had picked -- but Connect and Delete must
    /// not act on something that is not on the screen.
    fn selection_is_visible(&self) -> bool {
        match self.selected {
            Some(index) => self.visible().contains(&index),
            None => false,
        }
    }

    /// Move the selection `delta` rows through what the filter is showing.
    fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let here = self
            .selected
            .and_then(|index| visible.iter().position(|&i| i == index));
        let next = match here {
            // Arriving from nowhere -- no selection, or one the filter has
            // hidden -- lands on an end rather than jumping to the middle.
            None if delta < 0 => visible.len() - 1,
            None => 0,
            Some(row) => (row as isize + delta).clamp(0, visible.len() as isize - 1) as usize,
        };
        self.selected = Some(visible[next]);
        self.confirming_delete = None;
        self.scroll_to_selection = true;
    }

    // ---- actions -------------------------------------------------------

    /// Whether an action can be taken right now.
    ///
    /// The single gate for the menu, the accelerators and the buttons. Note
    /// that `load_failed` disables everything that would write the file --
    /// disabled rather than merely warned about, because the first New ->
    /// Save is what would replace a file full of connections with the one
    /// just typed.
    fn enabled(&self, action: &Action) -> bool {
        let selection = self.selection_is_visible();
        let writable = !self.load_failed;
        match action {
            Action::New | Action::AskImport => writable,
            Action::Duplicate => selection && writable,
            Action::AskDelete => selection && writable,
            // Exporting an empty list because the real one could not be read
            // would write a file that misrepresents what the user has.
            Action::AskExport => writable,
            Action::Connect | Action::Edit | Action::CopyCommandLine => selection,
            Action::MoveAside => self.load_failed,
            _ => true,
        }
    }

    /// Do one thing, if it is allowed.
    fn perform(&mut self, action: Action, ctx: &egui::Context) {
        if !self.enabled(&action) {
            return;
        }
        match action {
            Action::New => {
                let name = self.store.unique_name("New connection");
                self.open_editor(Profile::new(name), None);
            }
            Action::Duplicate => {
                let Some(source) = self.selected_profile().cloned() else {
                    return;
                };
                let mut copy = source.clone();
                copy.name = self.store.unique_name(&format!("{} copy", source.name));
                let name = copy.name.clone();
                self.store.upsert(copy);
                self.selected = self.store.position(&name);
                self.status = format!("Duplicated {} as {name}", source.name);
                self.error = None;
                self.save();
            }
            Action::AskDelete => {
                if let Some(name) = self.selected_name() {
                    self.dialog = Some(Dialog::ConfirmDelete(name));
                }
            }
            Action::AskImport => {
                // Deliberately *not* the live connections file. Import is the
                // other half of Export -- write on one machine, copy the file,
                // read on the other -- and a prefilled path to the file
                // already open would turn one Enter into a duplicate of every
                // connection the user has.
                self.dialog = Some(Dialog::Import(self.beside(EXCHANGE_FILE)));
            }
            Action::AskExport => {
                self.dialog = Some(Dialog::Export(self.beside(EXCHANGE_FILE)));
            }
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),

            Action::Connect => {
                if let Some(index) = self.selected {
                    self.connect(index);
                }
            }
            Action::Edit => {
                if let Some(profile) = self.selected_profile().cloned() {
                    let original = profile.name.clone();
                    self.open_editor(profile, Some(original));
                }
            }
            Action::CopyCommandLine => {
                if let Some(profile) = self.selected_profile() {
                    let line = format!("lynxrdp {}", shell_join(&profile.args()));
                    self.status = format!("Copied: {line}");
                    self.error = None;
                    ctx.copy_text(line);
                }
            }
            Action::CopyConnectionsPath => {
                let path = self.path.display().to_string();
                self.status = format!("Copied {path}");
                self.error = None;
                ctx.copy_text(path);
            }
            Action::Reload => self.reload(),
            Action::MoveAside => self.move_bad_file_aside(),

            Action::SetTheme(choice) => {
                self.settings.theme = choice;
                ctx.set_theme(egui::ThemePreference::from(choice));
                self.save_settings();
            }
            Action::ToggleCompactRows => {
                self.settings.compact_rows = !self.settings.compact_rows;
                self.save_settings();
            }
            Action::ToggleCommandLine => {
                self.settings.show_command_line = !self.settings.show_command_line;
                self.save_settings();
            }
            Action::Zoom(factor) => {
                let zoom = crate::settings::clamp_zoom(factor);
                self.settings.zoom = zoom;
                ctx.set_zoom_factor(zoom);
                self.save_settings();
            }

            Action::ShowShortcuts => self.dialog = Some(Dialog::Shortcuts),
            Action::ShowAbout => self.dialog = Some(Dialog::About),
            Action::OpenDocumentation => ctx.open_url(egui::OpenUrl::new_tab(REPO_URL)),

            Action::Move(delta) => self.move_selection(delta),
            Action::FocusFilter => self.focus_filter = true,
            Action::ClearFilter => self.filter.clear(),
        }
    }

    /// A path in the configuration directory, as a starting point for the
    /// import and export dialogs.
    fn beside(&self, name: &str) -> String {
        match self.path.parent() {
            Some(dir) => dir.join(name).display().to_string(),
            None => name.to_string(),
        }
    }

    fn open_editor(&mut self, profile: Profile, original: Option<String>) {
        self.view = View::Edit(Box::new(Editor::new(profile, original)));
        self.confirming_delete = None;
        self.error = None;
    }

    /// Merge another connections file into this one.
    ///
    /// Names collide by definition -- two machines both have a "work" -- so
    /// an incoming name that is taken is *renamed*, never allowed to replace
    /// what is already here. An import that silently overwrote a host would
    /// be indistinguishable from one that worked.
    fn import_from(&mut self, path: &Path) {
        // Importing the file that is already open would rename every
        // connection into a copy of itself and then save the result over the
        // original. Refused rather than merged: there is no reading of that
        // request that the user meant.
        if same_file(path, &self.path) {
            self.status.clear();
            self.error = Some(format!(
                "{} is the file this window is already showing; \
                 importing it would duplicate every connection.",
                path.display()
            ));
            return;
        }
        let incoming = match std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))
            .and_then(|text| Profiles::from_toml(&text))
        {
            Ok(profiles) => profiles,
            Err(e) => {
                self.status.clear();
                self.error = Some(format!("Could not import {}: {e:#}", path.display()));
                return;
            }
        };
        if incoming.items.is_empty() {
            self.status.clear();
            self.error = Some(format!("{} holds no connections.", path.display()));
            return;
        }
        // Checked before anything is added rather than as the list fills, so
        // an import that is too big is refused whole instead of half-applied.
        let total = self.store.items.len() + incoming.items.len();
        if total > profiles::MAX_PROFILES {
            self.status.clear();
            self.error = Some(format!(
                "Importing {} connections would make {total}, over the {} supported. \
                 Nothing was imported.",
                incoming.items.len(),
                profiles::MAX_PROFILES
            ));
            return;
        }
        let count = incoming.items.len();
        let mut renamed = 0;
        for mut profile in incoming.items {
            if self.store.position(&profile.name).is_some() {
                profile.name = self.store.unique_name(&profile.name);
                renamed += 1;
            }
            self.store.upsert(profile);
        }
        self.error = None;
        self.status = match renamed {
            0 => format!("Imported {}", plural(count, "connection")),
            n => format!("Imported {} ({n} renamed)", plural(count, "connection")),
        };
        self.save();
    }

    /// Write the whole list to another file.
    fn export_to(&mut self, path: &Path) {
        match self.store.save(path) {
            Ok(()) => {
                self.error = None;
                self.status = format!(
                    "Exported {} to {}",
                    plural(self.store.items.len(), "connection"),
                    path.display()
                );
            }
            Err(e) => {
                self.status.clear();
                self.error = Some(format!("Could not export to {}: {e:#}", path.display()));
            }
        }
    }

    fn delete_selected(&mut self) {
        let Some(index) = self.selected else { return };
        let Some(name) = self.store.items.get(index).map(|p| p.name.clone()) else {
            return;
        };
        self.store.remove(&name);
        self.confirming_delete = None;
        self.selected = if self.store.items.is_empty() {
            None
        } else {
            Some(index.min(self.store.items.len() - 1))
        };
        self.status = format!("Deleted {name}");
        self.save();
    }

    // ---- views ---------------------------------------------------------

    /// The menu bar. Every item does something; nothing here is a placeholder
    /// for a feature that does not exist yet.
    fn menu_ui(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut chosen = None;
        egui::menu::bar(ui, |ui| {
            self.file_menu(ui, &mut chosen);
            self.connection_menu(ui, &mut chosen);
            self.view_menu(ui, &mut chosen);
            self.help_menu(ui, &mut chosen);
        });
        chosen
    }

    /// One menu item, gated by [`Self::enabled`] and labelled with the key
    /// that does the same thing.
    fn item(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        action: Action,
        shortcut: Option<egui::KeyboardShortcut>,
        chosen: &mut Option<Action>,
    ) -> egui::Response {
        let mut button = egui::Button::new(label);
        if let Some(shortcut) = shortcut {
            button = button.shortcut_text(ui.ctx().format_shortcut(&shortcut));
        }
        let response = ui.add_enabled(self.enabled(&action), button);
        if response.clicked() {
            *chosen = Some(action);
            ui.close_menu();
        }
        response
    }

    fn file_menu(&mut self, ui: &mut egui::Ui, chosen: &mut Option<Action>) {
        ui.menu_button("File", |ui| {
            self.item(ui, "New Connection…", Action::New, Some(keys::NEW), chosen)
                .on_disabled_hover_text(
                    "The connections file could not be read; adding one now would replace it.",
                );
            self.item(
                ui,
                "Duplicate",
                Action::Duplicate,
                Some(keys::DUPLICATE),
                chosen,
            )
            .on_disabled_hover_text("Select a connection first.");
            self.item(ui, "Delete…", Action::AskDelete, Some(keys::DELETE), chosen)
                .on_disabled_hover_text("Select a connection first.");
            ui.separator();
            self.item(
                ui,
                "Import Connections…",
                Action::AskImport,
                Some(keys::IMPORT),
                chosen,
            );
            self.item(
                ui,
                "Export Connections…",
                Action::AskExport,
                Some(keys::EXPORT),
                chosen,
            )
            .on_disabled_hover_text(
                "The connections file could not be read, so an export would be empty.",
            );
            ui.separator();
            self.item(ui, "Quit", Action::Quit, Some(keys::QUIT), chosen)
                .on_hover_text(
                    "Running sessions keep running; closing the manager does not disconnect them.",
                );
        });
    }

    fn connection_menu(&mut self, ui: &mut egui::Ui, chosen: &mut Option<Action>) {
        ui.menu_button("Connection", |ui| {
            self.item(ui, "Connect", Action::Connect, Some(keys::CONNECT), chosen)
                .on_disabled_hover_text("Select a connection first.");
            self.item(ui, "Edit…", Action::Edit, Some(keys::EDIT), chosen)
                .on_disabled_hover_text("Select a connection first.");
            ui.separator();
            self.item(
                ui,
                "Copy Command Line",
                Action::CopyCommandLine,
                Some(keys::COPY_COMMAND),
                chosen,
            )
            .on_hover_text("The `lynxrdp` invocation this connection is equivalent to.");
            self.item(
                ui,
                "Copy Connections File Path",
                Action::CopyConnectionsPath,
                None,
                chosen,
            );
            ui.separator();
            self.item(
                ui,
                "Reload From Disk",
                Action::Reload,
                Some(keys::RELOAD),
                chosen,
            )
            .on_hover_text("Re-read the file, discarding nothing: saves happen immediately.");
            self.item(
                ui,
                "Move Broken File Aside…",
                Action::MoveAside,
                None,
                chosen,
            )
            .on_disabled_hover_text("The connections file reads fine; there is nothing to move.");
        });
    }

    fn view_menu(&mut self, ui: &mut egui::Ui, chosen: &mut Option<Action>) {
        let settings = self.settings.clone();
        ui.menu_button("View", |ui| {
            ui.menu_button("Theme", |ui| {
                for (choice, label) in [
                    (ThemeChoice::System, "System"),
                    (ThemeChoice::Light, "Light"),
                    (ThemeChoice::Dark, "Dark"),
                ] {
                    let response = ui.radio(settings.theme == choice, label);
                    if choice == ThemeChoice::System {
                        // Said plainly rather than implied: winit reports no
                        // theme on X11 and egui falls back to dark, so
                        // "System" is not universally what it sounds like.
                        response.clone().on_hover_text(
                            "Follows the desktop where it can be read. X11 does not report it, \
                             so System is Dark there.",
                        );
                    }
                    if response.clicked() {
                        *chosen = Some(Action::SetTheme(choice));
                        ui.close_menu();
                    }
                }
            });
            let mut compact = settings.compact_rows;
            if ui.checkbox(&mut compact, "Compact Rows").clicked() {
                *chosen = Some(Action::ToggleCompactRows);
                ui.close_menu();
            }
            let mut command_line = settings.show_command_line;
            if ui
                .checkbox(&mut command_line, "Show Command Line")
                .on_hover_text("The strip under the list shows what the selected row would run.")
                .clicked()
            {
                *chosen = Some(Action::ToggleCommandLine);
                ui.close_menu();
            }
            ui.separator();
            let zoom = ui.ctx().zoom_factor();
            self.item(
                ui,
                "Zoom In",
                Action::Zoom(zoom + theme::ZOOM_STEP),
                Some(keys::ZOOM_IN),
                chosen,
            );
            self.item(
                ui,
                "Zoom Out",
                Action::Zoom(zoom - theme::ZOOM_STEP),
                Some(keys::ZOOM_OUT),
                chosen,
            );
            self.item(
                ui,
                "Actual Size",
                Action::Zoom(1.0),
                Some(keys::ZOOM_RESET),
                chosen,
            );
        });
    }

    fn help_menu(&mut self, ui: &mut egui::Ui, chosen: &mut Option<Action>) {
        ui.menu_button("Help", |ui| {
            self.item(
                ui,
                "Keyboard Shortcuts…",
                Action::ShowShortcuts,
                Some(keys::SHORTCUTS),
                chosen,
            );
            self.item(ui, "Documentation", Action::OpenDocumentation, None, chosen)
                .on_hover_text(REPO_URL);
            ui.separator();
            self.item(ui, "About LynxRDP…", Action::ShowAbout, None, chosen);
        });
    }

    /// The heading, the filter box and New.
    fn toolbar_ui(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut chosen = None;
        let t = theme::of(ui.visuals());
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), theme::TOOLBAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space(theme::LIST_MARGIN);
                ui.heading("Saved connections");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(theme::LIST_MARGIN);
                    if ui
                        .add_enabled(self.enabled(&Action::New), theme::primary_button(&t, "New"))
                        .on_disabled_hover_text(
                            "The connections file could not be read; adding one now would \
                             replace it.",
                        )
                        .clicked()
                    {
                        chosen = Some(Action::New);
                    }
                    let filter = ui.add_sized(
                        [200.0, theme::CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut self.filter)
                            .id(egui::Id::new(FILTER_ID))
                            .font(egui::TextStyle::Monospace)
                            .hint_text("Filter"),
                    );
                    if self.focus_filter {
                        filter.request_focus();
                        self.focus_filter = false;
                    }
                });
            },
        );
        chosen
    }

    fn list_ui(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut chosen = self.toolbar_ui(ui);
        let t = theme::of(ui.visuals());
        hairline(ui, t.border);

        if self.load_failed {
            // An empty list here means "unreadable", not "none saved", and
            // the difference matters enough to say so where the list would
            // have been rather than only in the status line.
            padded(ui, |ui| {
                ui.add_space(theme::UNIT);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        egui::RichText::new(
                            "The connections file could not be read, so nothing is listed \
                             and saving is off.",
                        )
                        .color(t.warn),
                    );
                    if ui
                        .button("Move it aside")
                        .on_hover_text("Renames it to connections.toml.bad and starts empty.")
                        .clicked()
                    {
                        chosen = Some(Action::MoveAside);
                    }
                });
                ui.add_space(theme::UNIT);
            });
            hairline(ui, t.border);
        }

        if self.load_failed {
            // Nothing more to say below the banner: an empty list here is not
            // "none saved" and not "none matched", and offering either
            // explanation would be a third wrong answer.
            return chosen;
        }

        let visible = self.visible();
        if self.store.items.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(10.0 * theme::UNIT);
                ui.label(egui::RichText::new("No connections yet.").color(t.text_dim));
                // The actual key, formatted for this platform, rather than
                // "see the File menu": the point of an empty state is to say
                // what to do next without sending anyone hunting.
                let key = ui.ctx().format_shortcut(&keys::NEW);
                ui.label(
                    egui::RichText::new(format!("Choose New, or press {key}."))
                        .small()
                        .color(t.text_disabled),
                );
            });
            return chosen;
        }
        if visible.is_empty() {
            // Reachable only with something typed in the filter: an empty
            // store took the branch above.
            ui.vertical_centered(|ui| {
                ui.add_space(10.0 * theme::UNIT);
                ui.label(
                    egui::RichText::new(format!("Nothing matches {:?}.", self.filter.trim()))
                        .color(t.text_dim),
                );
                if ui.button("Clear the filter").clicked() {
                    chosen = Some(Action::ClearFilter);
                }
            });
            return chosen;
        }

        let compact = self.settings.compact_rows;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The gap between rows is the layout's item spacing rather
                // than a space each row adds after itself: egui inserts the
                // former between two allocations anyway, so doing both left
                // the rows ten points apart instead of four.
                ui.spacing_mut().item_spacing.y = theme::ROW_GAP;
                ui.add_space(theme::UNIT);
                for index in visible {
                    let response = self.row_ui(ui, index, compact);
                    if response.clicked() {
                        self.selected = Some(index);
                        self.confirming_delete = None;
                    }
                    // Double-click is the fastest way in, and the one people
                    // will try first.
                    if response.double_clicked() {
                        self.selected = Some(index);
                        chosen = Some(Action::Connect);
                    }
                    // Tab moves the selection with the focus rather than
                    // leaving the two on different rows. Without this, Enter
                    // -- which is consumed globally, before any widget sees
                    // it -- would connect to whatever was selected before,
                    // while the ring sat somewhere else.
                    if response.gained_focus() {
                        self.selected = Some(index);
                        self.confirming_delete = None;
                    }
                    if self.scroll_to_selection && self.selected == Some(index) {
                        response.scroll_to_me(None);
                    }
                }
                ui.add_space(theme::UNIT);
            });
        self.scroll_to_selection = false;
        chosen
    }

    /// One connection, painted by hand.
    ///
    /// Not a `SelectableLabel`: that widget sizes itself to its text and
    /// centres it, so only the words would be clickable and the names would
    /// wander with their length. A list is read down its left edge, and the
    /// whole row should be the target.
    fn row_ui(&self, ui: &mut egui::Ui, index: usize, compact: bool) -> egui::Response {
        let profile = &self.store.items[index];
        let (name, destination, detail) = (
            profile.name.clone(),
            profile.destination(),
            row_detail(profile),
        );
        let selected = self.selected == Some(index);
        let dark = ui.visuals().dark_mode;
        let t = theme::of(ui.visuals());

        let height = if compact {
            theme::ROW_HEIGHT_COMPACT
        } else {
            theme::ROW_HEIGHT
        };
        // Interacted with an id derived from the connection's position rather
        // than with the automatic one, which is derived from where the widget
        // landed: with that, typing in the filter box would move the keyboard
        // focus from the row it was on to whatever row slid into its place.
        // `Sense::click` is already CLICK | FOCUSABLE, which is what puts rows
        // on the Tab path; all that was ever missing was a visible ring.
        let (_, outer) = ui.allocate_space(egui::vec2(ui.available_width(), height));
        let response = ui.interact(outer, row_id(index), egui::Sense::click());
        if !ui.is_rect_visible(outer) {
            return response;
        }
        let rect = egui::Rect::from_min_max(
            egui::pos2(outer.left() + theme::LIST_MARGIN, outer.top()),
            egui::pos2(outer.right() - theme::LIST_MARGIN, outer.bottom()),
        );
        let radius = egui::CornerRadius::same(theme::RADIUS);
        let painter = ui.painter();
        if selected {
            painter.rect_filled(rect, radius, t.accent_weak);
            // Fill *and* an edge. The fill alone is 1.34:1 against the
            // surface in dark and 1.08:1 in light -- a state nobody should be
            // asked to see by colour alone.
            painter.rect_filled(
                egui::Rect::from_min_max(
                    rect.left_top(),
                    egui::pos2(rect.left() + theme::SELECTED_EDGE, rect.bottom()),
                ),
                egui::CornerRadius {
                    nw: theme::RADIUS,
                    sw: theme::RADIUS,
                    ne: 0,
                    se: 0,
                },
                t.accent,
            );
        } else if response.hovered() {
            painter.rect_filled(rect, radius, t.hover_fill);
        }
        if response.has_focus() {
            painter.rect_stroke(
                rect.shrink(1.0),
                radius,
                egui::Stroke::new(theme::FOCUS_RING, theme::focus_ring(&t, dark)),
                egui::StrokeKind::Inside,
            );
        }

        let left = outer.left() + theme::ROW_TEXT_INSET;
        let right_limit = rect.right() - theme::LIST_MARGIN;
        if compact {
            // One line: the name leads, the address is the identity and is
            // therefore the half that must survive. When they collide it is
            // the name that gets an ellipsis.
            let address = one_line(
                ui,
                &destination,
                theme::row_detail_font(),
                t.text_dim,
                f32::INFINITY,
            );
            let room = (right_limit - left - address.size().x - theme::LIST_MARGIN).max(0.0);
            let title = one_line(ui, &name, theme::row_compact_title_font(), t.text, room);
            let painter = ui.painter();
            painter.galley(
                egui::pos2(left, rect.center().y - title.size().y / 2.0),
                title,
                t.text,
            );
            painter.galley(
                egui::pos2(
                    right_limit - address.size().x,
                    rect.center().y - address.size().y / 2.0,
                ),
                address,
                t.text_dim,
            );
        } else {
            // Two lines, and a right-hand column for the things that differ
            // from the defaults. Aligned across rows because it is laid out
            // from the right edge, not after the address.
            let detail = one_line(
                ui,
                &detail,
                theme::row_detail_font(),
                t.text_dim,
                f32::INFINITY,
            );
            let room = (right_limit - left - detail.size().x - theme::LIST_MARGIN).max(0.0);
            let title = one_line(ui, &name, theme::row_title_font(), t.text, room);
            let address = one_line(
                ui,
                &destination,
                theme::row_address_font(),
                t.text_dim,
                room,
            );
            let block = title.size().y + address.size().y;
            let top = rect.center().y - block / 2.0;
            let address_y = top + title.size().y;
            let detail_y = rect.center().y - detail.size().y / 2.0;
            let detail_x = right_limit - detail.size().x;
            let painter = ui.painter();
            painter.galley(egui::pos2(left, top), title, t.text);
            painter.galley(egui::pos2(left, address_y), address, t.text_dim);
            painter.galley(egui::pos2(detail_x, detail_y), detail, t.text_dim);
        }
        response
    }

    /// The strip that shows what the selected row would run.
    ///
    /// The "the GUI and the command line must not drift" invariant made
    /// visible, and free: `args()` is what the launcher actually spawns.
    fn command_line_ui(&mut self, ui: &mut egui::Ui) {
        let t = theme::of(ui.visuals());
        let line = match self.selected_profile() {
            Some(profile) if self.selection_is_visible() => {
                format!("lynxrdp {}", shell_join(&profile.args()))
            }
            _ => "lynxrdp — select a connection".to_string(),
        };
        padded(ui, |ui| {
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), theme::COMMAND_STRIP_HEIGHT),
                egui::Sense::hover(),
            );
            ui.painter().rect(
                rect,
                egui::CornerRadius::same(theme::RADIUS),
                t.surface_sunken,
                egui::Stroke::new(1.0, t.border),
                egui::StrokeKind::Inside,
            );
            let galley = one_line(
                ui,
                &line,
                theme::row_detail_font(),
                t.text_dim,
                f32::INFINITY,
            );
            // Clipped rather than ellipsised: the interesting end of a long
            // invocation is the start, and a scroll bar on a read-only strip
            // is one more thing to hit by accident.
            ui.painter()
                .with_clip_rect(rect.shrink(2.0 * theme::UNIT))
                .galley(
                    egui::pos2(
                        rect.left() + 2.0 * theme::UNIT,
                        rect.center().y - galley.size().y / 2.0,
                    ),
                    galley,
                    t.text_dim,
                );
        });
    }

    /// Connect, Edit, Delete.
    fn actions_ui(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut chosen = None;
        let t = theme::of(ui.visuals());
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), theme::ACTION_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space(theme::LIST_MARGIN);
                if ui
                    .add_enabled(
                        self.enabled(&Action::Connect),
                        theme::primary_button(&t, "Connect"),
                    )
                    .clicked()
                {
                    chosen = Some(Action::Connect);
                }
                if ui
                    .add_enabled(self.enabled(&Action::Edit), egui::Button::new("Edit"))
                    .clicked()
                {
                    chosen = Some(Action::Edit);
                }
                // Two clicks rather than a modal, unchanged: the menu item is
                // the one that asks in a dialog. Two paths, one confirmation
                // each.
                let confirming = self
                    .selected_profile()
                    .map(|p| self.confirming_delete.as_deref() == Some(p.name.as_str()))
                    .unwrap_or(false);
                let label = if confirming {
                    egui::RichText::new("Really delete?").color(t.danger)
                } else {
                    egui::RichText::new("Delete")
                };
                if ui
                    .add_enabled(self.enabled(&Action::AskDelete), egui::Button::new(label))
                    .clicked()
                {
                    if confirming {
                        self.delete_selected();
                    } else {
                        self.confirming_delete = self.selected_name();
                    }
                }
            },
        );
        chosen
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let t = theme::of(ui.visuals());
        let View::Edit(editor) = &mut self.view else {
            return;
        };
        let adding = editor.original.is_none();

        // Scrolled, because the form is taller than the smallest window a
        // user is allowed to make. Save and Cancel are not in here for the
        // same reason: they live in a bar of their own, so the two things a
        // half-filled form needs are never the parts below the fold.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                padded(ui, |ui| {
                    ui.add_space(theme::UNIT);
                    ui.heading(if adding {
                        "New connection"
                    } else {
                        "Edit connection"
                    });
                    ui.add_space(2.0 * theme::UNIT);

                    egui::Grid::new("connection")
                        .num_columns(2)
                        .spacing([4.0 * theme::UNIT, 2.5 * theme::UNIT])
                        .show(ui, |ui| {
                            // The name is prose; everything else is a machine string
                            // that a user has to be able to compare character by
                            // character, so it is monospace.
                            field(ui, &t, "Name");
                            ui.add_sized(
                                [320.0, theme::CONTROL_HEIGHT],
                                egui::TextEdit::singleline(&mut editor.profile.name),
                            );
                            ui.end_row();

                            field(ui, &t, "Host");
                            mono(ui, &mut editor.profile.host, 320.0, "");
                            ui.end_row();

                            field(ui, &t, "User");
                            mono(ui, &mut editor.profile.user, 200.0, "your SSH user");
                            ui.end_row();

                            field(ui, &t, "SSH port");
                            mono(ui, &mut editor.ssh_port, 96.0, "default");
                            ui.end_row();

                            field(ui, &t, "Identity file");
                            mono(
                                ui,
                                &mut editor.identity,
                                400.0,
                                "ssh -i, if not the default",
                            );
                            ui.end_row();

                            field(ui, &t, "Screen size");
                            mono(ui, &mut editor.size, 160.0, "1920x1080");
                            ui.end_row();

                            field(ui, &t, "LynxRDP port");
                            mono(
                                ui,
                                &mut editor.remote_port,
                                96.0,
                                &lynxrdp_proto::DEFAULT_PORT.to_string(),
                            );
                            ui.end_row();

                            field(ui, &t, "SSH options");
                            ui.add_sized(
                                [400.0, 3.0 * theme::CONTROL_HEIGHT],
                                egui::TextEdit::multiline(&mut editor.ssh_options)
                                    .font(egui::TextStyle::Monospace)
                                    .hint_text("one per line, e.g. ProxyJump=bastion")
                                    .desired_rows(3),
                            );
                            ui.end_row();
                        });

                    ui.add_space(2.5 * theme::UNIT);
                    egui::Frame::NONE
                        .stroke(egui::Stroke::new(1.0, t.border))
                        .corner_radius(egui::CornerRadius::same(theme::RADIUS))
                        .inner_margin(egui::Margin::same(12))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Session").small().color(t.text_dim));
                            ui.add_space(theme::UNIT);
                            ui.checkbox(&mut editor.profile.fullscreen, "Start fullscreen");
                            ui.checkbox(
                                &mut editor.profile.dynamic_resize,
                                "Resize the remote screen with the window",
                            );
                            ui.checkbox(&mut editor.profile.clipboard, "Share the clipboard");
                        });

                    if let Some(problem) = &editor.problem {
                        ui.add_space(theme::UNIT);
                        ui.add(
                            egui::Label::new(egui::RichText::new(problem.as_str()).color(t.danger))
                                .wrap(),
                        );
                    }

                    ui.add_space(2.0 * theme::UNIT);
                });
            });
    }

    /// Save and Cancel, in a bar of their own under the form.
    ///
    /// Returns `Some(true)` to save and `Some(false)` to abandon. The order
    /// is the one this window has always used; a platform-conditional swap
    /// would cost more in muscle memory than it bought in convention.
    fn editor_bar_ui(&mut self, ui: &mut egui::Ui) -> Option<bool> {
        let mut choice = None;
        let t = theme::of(ui.visuals());
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), theme::ACTION_BAR_HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space(theme::LIST_MARGIN);
                if ui.add(theme::primary_button(&t, "Save")).clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            },
        );
        choice
    }

    /// Leave the editor without writing anything.
    fn cancel_editor(&mut self) {
        self.view = View::List;
        self.error = None;
    }

    fn save_editor(&mut self) {
        let View::Edit(editor) = &self.view else {
            return;
        };
        let outcome = match editor.collect() {
            Err(problem) => Err(problem),
            Ok(profile) => {
                // Checked before anything is written, and before the status
                // line claims success. Entries are keyed by name, so saving
                // onto a name already in use would not add a connection --
                // upsert would replace the other one, and the host that was
                // there would be gone with no way back.
                if self
                    .store
                    .name_taken(&profile.name, editor.original.as_deref())
                {
                    Err(format!(
                        "There is already a connection called {:?}. Names have to be unique, \
                         so choose another.",
                        profile.name
                    ))
                } else {
                    Ok(profile)
                }
            }
        };
        match outcome {
            Err(problem) => {
                self.error = Some(problem.clone());
                if let View::Edit(editor) = &mut self.view {
                    editor.problem = Some(problem);
                }
            }
            Ok(profile) => {
                let original = match &self.view {
                    View::Edit(editor) => editor.original.clone(),
                    View::List => None,
                };
                // A rename leaves the old entry behind unless it is removed,
                // because entries are keyed by name.
                if let Some(previous) = original {
                    if previous != profile.name {
                        self.store.remove(&previous);
                    }
                }
                let name = profile.name.clone();
                self.store.upsert(profile);
                self.selected = self.store.position(&name);
                self.status = format!("Saved {name}");
                self.error = None;
                self.view = View::List;
                self.save();
            }
        }
    }

    /// The bar along the bottom: how many sessions are open, and the latest
    /// thing the launcher has to say.
    ///
    /// Split out of `update` so it can be laid out in a test. egui itself
    /// needs no window -- only eframe does -- so a bare `egui::Context` will
    /// run this and report where each piece of text actually landed, which
    /// is the only way to check a layout on a machine with no display.
    fn status_ui(&mut self, ui: &mut egui::Ui) {
        let t = theme::of(ui.visuals());
        // The count is placed first, from the right, so a long message
        // cannot push it off the edge: a failed connection brings several
        // lines of whatever SSH had to say.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            let running = self.sessions.count();
            if running > 0 {
                ui.label(
                    egui::RichText::new(plural(running, "session"))
                        .small()
                        .color(t.text_dim),
                );
                // A dot as well as a number, so "something is running" is
                // legible at a glance and not only on a reread. Allocated a
                // whole line tall and centred in it, because the layout is
                // top-aligned and a six-point box would sit level with the
                // tops of the letters rather than with their middles.
                let line = ui.text_style_height(&egui::TextStyle::Small);
                let (dot, _) = ui.allocate_exact_size(egui::vec2(6.0, line), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.0, t.ok);
            }
            let message = match &self.error {
                Some(error) => egui::RichText::new(error.as_str())
                    .small()
                    .color(ui.visuals().error_fg_color),
                None => egui::RichText::new(self.status.as_str())
                    .small()
                    .color(t.text_dim),
            };
            // Into a left-to-right region filling what is left, rather than
            // straight into this one. A Label takes its horizontal placement
            // from the layout it is added to, so added here it would be
            // right-aligned: "Saved work" would sit against the far edge of
            // the window next to the count, and a wrapped SSH error would
            // come out with a ragged left margin.
            let rest = egui::vec2(ui.available_width(), 0.0);
            ui.allocate_ui_with_layout(rest, egui::Layout::left_to_right(egui::Align::TOP), |ui| {
                ui.add(egui::Label::new(message).wrap())
            });
        });
    }

    /// Whatever modal is up. Returns the action it asked for, if any.
    fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let mut close = false;
        let t = theme::of(&ctx.style().visuals);
        // What a scrolling dialog may take before its own buttons are pushed
        // out of the window: everything but the heading, the buttons and the
        // modal's margins.
        let room = (ctx.screen_rect().height() - 44.0 * theme::UNIT).max(30.0 * theme::UNIT);
        let response = egui::Modal::new(egui::Id::new("lynxrdp-dialog")).show(ctx, |ui| {
            ui.set_max_width(520.0);
            let confirmed = ui.input(|i| i.key_pressed(egui::Key::Enter));
            match &mut dialog {
                Dialog::ConfirmDelete(name) => {
                    ui.heading("Delete connection");
                    ui.add_space(theme::UNIT);
                    ui.label(format!("Delete {name:?}?"));
                    ui.label(
                        egui::RichText::new(
                            "The connection is removed from the file; \
                                             running sessions are not affected.",
                        )
                        .small()
                        .color(t.text_dim),
                    );
                    ui.add_space(2.5 * theme::UNIT);
                    ui.horizontal(|ui| {
                        // Both answers are collected before either is acted
                        // on, and Cancel wins. egui activates a *focused*
                        // widget on Enter, so a user who tabs to Cancel and
                        // presses Enter produces a Cancel click and the bare
                        // Enter that stands for "confirm" in the same frame:
                        // acting on the second would delete the connection
                        // they just asked to keep.
                        let delete = ui
                            .add(
                                egui::Button::new(egui::RichText::new("Delete").color(t.on_accent))
                                    .fill(t.danger),
                            )
                            .clicked();
                        let cancel = ui.button("Cancel").clicked();
                        if cancel {
                            close = true;
                        } else if delete || confirmed {
                            self.delete_selected();
                            close = true;
                        }
                    });
                }
                Dialog::Import(path) => {
                    ui.heading("Import connections");
                    ui.add_space(theme::UNIT);
                    ui.label(
                        egui::RichText::new(
                            "Names already in use are renamed, never replaced, so an import \
                             cannot lose a host you already have.",
                        )
                        .small()
                        .color(t.text_dim),
                    );
                    ui.add_space(theme::UNIT);
                    path_field(ui, path);
                    ui.add_space(2.5 * theme::UNIT);
                    ui.horizontal(|ui| {
                        // Cancel first, for the reason spelled out under the
                        // delete dialog: Enter on a focused Cancel is both a
                        // click and a confirmation.
                        let go = ui.add(theme::primary_button(&t, "Import")).clicked();
                        let cancel = ui.button("Cancel").clicked();
                        if cancel {
                            close = true;
                        } else if go || confirmed {
                            let path = PathBuf::from(path.trim());
                            self.import_from(&path);
                            close = true;
                        }
                    });
                }
                Dialog::Export(path) => {
                    ui.heading("Export connections");
                    ui.add_space(theme::UNIT);
                    ui.label(
                        egui::RichText::new(
                            "Contains hosts, users, ports and identity paths. No passwords \
                             — there are none to export.",
                        )
                        .small()
                        .color(t.text_dim),
                    );
                    ui.add_space(theme::UNIT);
                    path_field(ui, path);
                    ui.add_space(2.5 * theme::UNIT);
                    ui.horizontal(|ui| {
                        let go = ui.add(theme::primary_button(&t, "Export")).clicked();
                        let cancel = ui.button("Cancel").clicked();
                        if cancel {
                            close = true;
                        } else if go || confirmed {
                            let path = PathBuf::from(path.trim());
                            self.export_to(&path);
                            close = true;
                        }
                    });
                }
                Dialog::Shortcuts => {
                    ui.heading("Keyboard shortcuts");
                    ui.add_space(2.0 * theme::UNIT);
                    // Scrolled and capped: the list is longer than the
                    // smallest window is tall, and a modal that runs off both
                    // ends takes its own Close button with it.
                    egui::ScrollArea::vertical()
                        .max_height(room)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            egui::Grid::new("shortcuts")
                                .num_columns(2)
                                .spacing([4.0 * theme::UNIT, 1.5 * theme::UNIT])
                                .show(ui, |ui| {
                                    for (label, keys) in shortcut_list(ctx) {
                                        ui.label(egui::RichText::new(label).color(t.text_dim));
                                        ui.label(
                                            egui::RichText::new(keys).monospace().color(t.text),
                                        );
                                        ui.end_row();
                                    }
                                });
                        });
                    ui.add_space(2.5 * theme::UNIT);
                    if ui.button("Close").clicked() || confirmed {
                        close = true;
                    }
                }
                Dialog::About => {
                    ui.heading("LynxRDP");
                    ui.label(
                        egui::RichText::new(format!(
                            "version {} · {}",
                            env!("CARGO_PKG_VERSION"),
                            crate::CLIENT_NAME
                        ))
                        .small()
                        .color(t.text_dim),
                    );
                    ui.add_space(2.0 * theme::UNIT);
                    ui.label("Connections are saved in");
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(self.path.display().to_string())
                                .monospace()
                                .color(t.text_dim),
                        )
                        .selectable(true)
                        .wrap(),
                    );
                    ui.add_space(2.0 * theme::UNIT);
                    ui.add(
                        egui::Label::new(
                            "Saved connections hold hosts, users, ports and identity paths. They \
                         never hold a password or passphrase: SSH owns authentication.",
                        )
                        .wrap(),
                    );
                    ui.add_space(2.0 * theme::UNIT);
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(env!("CARGO_PKG_LICENSE")).color(t.text_dim));
                        ui.hyperlink_to(REPO_URL, REPO_URL);
                    });
                    ui.add_space(2.5 * theme::UNIT);
                    if ui.button("Close").clicked() || confirmed {
                        close = true;
                    }
                }
            }
        });
        if !close && !response.should_close() {
            self.dialog = Some(dialog);
        }
    }

    /// Consume the accelerators.
    ///
    /// At the top of the frame and only while the list is showing, so a
    /// shortcut cannot fire while the user is typing a host name into the
    /// editor, and only while no modal is up, so Enter belongs to the dialog.
    fn accelerators(&self, ctx: &egui::Context) -> Option<Action> {
        if !matches!(self.view, View::List) || self.dialog.is_some() {
            return None;
        }
        let focused = ctx.memory(|m| m.focused());
        let typing = focused == Some(egui::Id::new(FILTER_ID));
        // Enter is the connect key only where nothing else has a claim on
        // it. egui activates a *focused* widget on Enter as well as on Space,
        // so taking Enter here unconditionally left a keyboard user unable to
        // press New, Edit or Delete at all: Tab to the button, press Enter,
        // and the launcher connected to whatever row was selected instead.
        // The two places Enter really is ours are the ones the design asks
        // for -- a focused row, and the filter box, where Enter is how you
        // connect to what you have just narrowed down to.
        let enter_is_ours = match focused {
            None => true,
            Some(id) => {
                id == egui::Id::new(FILTER_ID)
                    || (0..self.store.items.len()).any(|index| row_id(index) == id)
            }
        };
        let zoom = ctx.zoom_factor();
        ctx.input_mut(|i| {
            for (shortcut, action) in [
                (keys::NEW, Action::New),
                (keys::DUPLICATE, Action::Duplicate),
                (keys::IMPORT, Action::AskImport),
                (keys::EXPORT, Action::AskExport),
                (keys::QUIT, Action::Quit),
                (keys::CONNECT, Action::Connect),
                (keys::EDIT, Action::Edit),
                (keys::COPY_COMMAND, Action::CopyCommandLine),
                (keys::RELOAD, Action::Reload),
                (keys::ZOOM_IN, Action::Zoom(zoom + theme::ZOOM_STEP)),
                (keys::ZOOM_OUT, Action::Zoom(zoom - theme::ZOOM_STEP)),
                (keys::ZOOM_RESET, Action::Zoom(1.0)),
                (keys::SHORTCUTS, Action::ShowShortcuts),
            ] {
                if i.consume_shortcut(&shortcut) {
                    return Some(action);
                }
            }
            // Up and down are taken before any widget sees them, which is
            // what lets the arrows walk the list while the caret is still in
            // the filter box -- type three characters, arrow down, Enter.
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                return Some(Action::Move(1));
            }
            if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                return Some(Action::Move(-1));
            }
            if !typing {
                // Delete removes a connection unless it is removing a
                // character, and `/` is only a jump when it is not text.
                if i.consume_shortcut(&keys::DELETE) || i.consume_shortcut(&keys::DELETE_ALT) {
                    return Some(Action::AskDelete);
                }
                if i.consume_shortcut(&keys::FILTER) {
                    return Some(Action::FocusFilter);
                }
            }
            if enter_is_ours && i.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                return Some(Action::Connect);
            }
            None
        })
    }
}

impl eframe::App for Launcher {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

impl Launcher {
    /// One frame, everything but the `eframe` plumbing.
    ///
    /// Split out for the same reason `status_ui` is: an `eframe::Frame` needs
    /// a real window, and a bare `egui::Context` does not. This is what the
    /// layout tests below run, so what they measure is what ships rather than
    /// a reconstruction of it.
    fn draw(&mut self, ctx: &egui::Context) {
        self.sessions.reap();
        self.drain_failures();

        // Accelerators are answered *before* the panels are built, not with
        // the buttons afterwards. A key that only took effect on the next
        // frame would sit there until something else caused a repaint --
        // half a second, by the timer at the bottom of this function -- which
        // is exactly long enough to feel broken when it is `/` and you have
        // already started typing.
        let mut pressed = self.accelerators(ctx);
        // Escape empties the filter rather than closing the window: a list
        // filtered down to nothing looks broken, and this is the way out.
        if matches!(self.view, View::List)
            && self.dialog.is_none()
            && !self.filter.is_empty()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            pressed = Some(Action::ClearFilter);
        }
        if let Some(action) = pressed {
            self.perform(action, ctx);
        }
        let mut action = None;

        let t = theme::of(&ctx.style().visuals);
        let menubar = egui::TopBottomPanel::top("menubar")
            .frame(
                egui::Frame::NONE
                    .fill(t.surface)
                    .inner_margin(egui::Margin::symmetric(6, 4)),
            )
            .show(ctx, |ui| self.menu_ui(ui));
        action = action.or(menubar.inner);
        // The bar and the window below it are the same colour, so the seam
        // needs a line to exist at all.
        let seam = menubar.response.rect;
        ctx.layer_painter(egui::LayerId::background()).hline(
            seam.x_range(),
            seam.bottom() - 0.5,
            egui::Stroke::new(1.0, t.border),
        );

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_ui(ui));

        let mut editor_choice = None;
        if matches!(self.view, View::Edit(_))
            && self.dialog.is_none()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            editor_choice = Some(false);
        }

        if matches!(self.view, View::Edit(_)) {
            let bar = egui::TopBottomPanel::bottom("editor-actions")
                .frame(
                    egui::Frame::NONE
                        .fill(t.surface)
                        .inner_margin(egui::Margin::ZERO),
                )
                .show(ctx, |ui| {
                    hairline(ui, t.border);
                    self.editor_bar_ui(ui)
                });
            editor_choice = editor_choice.or(bar.inner);
        }

        if matches!(self.view, View::List) {
            let actions = egui::TopBottomPanel::bottom("actions")
                .frame(
                    egui::Frame::NONE
                        .fill(t.surface)
                        .inner_margin(egui::Margin::ZERO),
                )
                .show(ctx, |ui| {
                    hairline(ui, t.border);
                    self.actions_ui(ui)
                });
            action = action.or(actions.inner);
            if self.settings.show_command_line {
                egui::TopBottomPanel::bottom("commandline")
                    .frame(
                        egui::Frame::NONE
                            .fill(t.surface)
                            .inner_margin(egui::Margin::symmetric(0, 6)),
                    )
                    .show(ctx, |ui| self.command_line_ui(ui));
            }
        }

        let central = egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(t.surface)
                    // No side margin: rows run the full width so the hover
                    // and selection fills read as a list rather than as a
                    // column of cards, and each row insets itself.
                    .inner_margin(egui::Margin {
                        left: 0,
                        right: 0,
                        top: 4,
                        bottom: 0,
                    }),
            )
            .show(ctx, |ui| match self.view {
                View::List => self.list_ui(ui),
                View::Edit(_) => {
                    self.editor_ui(ui);
                    None
                }
            });
        action = action.or(central.inner);

        self.dialog_ui(ctx);

        match editor_choice {
            Some(true) => self.save_editor(),
            Some(false) => self.cancel_editor(),
            None => {}
        }
        if let Some(action) = action {
            self.perform(action, ctx);
        }

        // A session may exit at any time and the count in the status bar
        // should notice without needing a mouse move.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

// ---- small painting helpers ----------------------------------------------

/// Whether two paths name the same file.
///
/// Canonicalised where both exist, because `../cfg/connections.toml` and
/// `connections.toml` are the same file and a string comparison would say
/// otherwise. A path that cannot be canonicalised -- most often because it
/// does not exist -- falls back to comparing what was typed.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The stable id of one connection row.
fn row_id(index: usize) -> egui::Id {
    egui::Id::new(("lynxrdp-row", index))
}

/// A one-point separator across the current `Ui`.
fn hairline(ui: &mut egui::Ui, colour: egui::Color32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().hline(
        rect.x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, colour),
    );
}

/// Run `contents` inset by the list margin, so text lines up with the rows.
fn padded<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(theme::LIST_MARGIN as i8, 0))
        .show(ui, contents)
        .inner
}

/// Lay out one line, ellipsised if it does not fit `max_width`.
fn one_line(
    ui: &egui::Ui,
    text: &str,
    font: egui::FontId,
    colour: egui::Color32,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: font,
            color: colour,
            ..Default::default()
        },
    );
    job.break_on_newline = false;
    job.wrap = egui::text::TextWrapping {
        max_width,
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    ui.fonts(|f| f.layout_job(job))
}

/// The right-hand column of a row: what this connection does differently.
fn row_detail(profile: &Profile) -> String {
    let mut parts = Vec::new();
    if let Some(port) = profile.ssh_port {
        parts.push(format!(":{port}"));
    }
    if let Some((w, h)) = profile.size {
        parts.push(format!("{w}x{h}"));
    }
    parts.join("  ")
}

/// A right-aligned form label.
fn field(ui: &mut egui::Ui, t: &theme::Tokens, label: &str) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(egui::RichText::new(label).color(t.text_dim));
    });
}

/// A monospace form field of a fixed width.
///
/// Sized explicitly rather than left to `TextEdit`, which measures about 15
/// points tall and looks broken beside a 28-point button.
fn mono(ui: &mut egui::Ui, text: &mut String, width: f32, hint: &str) {
    ui.add_sized(
        [width, theme::CONTROL_HEIGHT],
        egui::TextEdit::singleline(text)
            .font(egui::TextStyle::Monospace)
            .hint_text(hint),
    );
}

/// The path box in the import and export dialogs.
///
/// Typed rather than picked. There is no native file dialog here and there
/// will not be one: `rfd` pulls in GTK or a desktop portal on Linux, and this
/// client is pure Rust with no C library dependencies on purpose.
fn path_field(ui: &mut egui::Ui, path: &mut String) {
    ui.add_sized(
        [440.0, theme::CONTROL_HEIGHT],
        egui::TextEdit::singleline(path).font(egui::TextStyle::Monospace),
    );
}

/// "1 session" / "3 sessions".
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// What the Help > Keyboard Shortcuts modal lists.
///
/// Includes the keys that are not menu items -- the arrows, `/`, Escape --
/// because those are exactly the ones a user cannot discover by opening a
/// menu.
fn shortcut_list(ctx: &egui::Context) -> Vec<(&'static str, String)> {
    let key = |shortcut: &egui::KeyboardShortcut| ctx.format_shortcut(shortcut);
    vec![
        ("Connect", format!("{}, Enter", key(&keys::CONNECT))),
        ("Edit", key(&keys::EDIT)),
        ("New connection", key(&keys::NEW)),
        ("Duplicate", key(&keys::DUPLICATE)),
        ("Delete", key(&keys::DELETE)),
        ("Move through the list", "Up, Down".into()),
        ("Filter the list", "/".into()),
        ("Clear the filter", "Esc".into()),
        ("Copy command line", key(&keys::COPY_COMMAND)),
        ("Reload from disk", key(&keys::RELOAD)),
        ("Import connections", key(&keys::IMPORT)),
        ("Export connections", key(&keys::EXPORT)),
        (
            "Zoom",
            format!(
                "{}, {}, {}",
                key(&keys::ZOOM_IN),
                key(&keys::ZOOM_OUT),
                key(&keys::ZOOM_RESET)
            ),
        ),
        ("This list", key(&keys::SHORTCUTS)),
        ("Quit", key(&keys::QUIT)),
    ]
}

/// Where the launcher reads and writes its connections.
pub fn default_path() -> Result<PathBuf> {
    Profiles::default_path().context("finding the connections file")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_parse_or_explain() {
        assert_eq!(parse_port("", "SSH port"), Ok(None));
        assert_eq!(parse_port("  ", "SSH port"), Ok(None));
        assert_eq!(parse_port("2222", "SSH port"), Ok(Some(2222)));
        assert!(parse_port("0", "SSH port").is_err());
        assert!(parse_port("70000", "SSH port").is_err());
        assert!(parse_port("nope", "SSH port").is_err());
        // The message names the field so the user knows which one to fix.
        assert!(parse_port("x", "LynxRDP port")
            .unwrap_err()
            .contains("LynxRDP port"));
    }

    #[test]
    fn sizes_parse_or_explain() {
        assert_eq!(parse_size(""), Ok(None));
        assert_eq!(parse_size("1920x1080"), Ok(Some((1920, 1080))));
        assert_eq!(parse_size(" 1280 X 720 "), Ok(Some((1280, 720))));
        assert!(parse_size("1920").is_err());
        assert!(parse_size("0x1080").is_err());
        assert!(parse_size("axb").is_err());
        assert!(parse_size("1920x").is_err());
    }

    fn editor_with(profile: Profile) -> Editor {
        Editor::new(profile, None)
    }

    #[test]
    fn an_editor_starts_from_the_profile() {
        let mut p = Profile::new("w");
        p.host = "h".into();
        p.ssh_port = Some(2222);
        p.size = Some((800, 600));
        p.ssh_options = vec!["A=1".into(), "B=2".into()];
        p.identity = Some(PathBuf::from("/k/id"));
        let e = editor_with(p);
        assert_eq!(e.ssh_port, "2222");
        assert_eq!(e.size, "800x600");
        assert_eq!(e.ssh_options, "A=1\nB=2");
        assert_eq!(e.identity, "/k/id");
    }

    #[test]
    fn unset_optional_fields_start_blank() {
        // Blank means "use the default", so these must not read as "0".
        let e = editor_with(Profile::new("w"));
        assert!(e.ssh_port.is_empty());
        assert!(e.remote_port.is_empty());
        assert!(e.size.is_empty());
        assert!(e.identity.is_empty());
        assert!(e.ssh_options.is_empty());
    }

    #[test]
    fn collect_folds_the_text_fields_back_in() {
        let mut p = Profile::new("  spaced  ");
        p.host = "  host  ".into();
        p.user = " alice ".into();
        let mut e = editor_with(p);
        e.ssh_port = "2222".into();
        e.size = "1024x768".into();
        e.ssh_options = "A=1\n\n  B=2  \n".into();
        e.identity = " /k/id ".into();
        let out = e.collect().unwrap();
        // Surrounding whitespace would otherwise reach ssh.
        assert_eq!(out.name, "spaced");
        assert_eq!(out.host, "host");
        assert_eq!(out.user, "alice");
        assert_eq!(out.ssh_port, Some(2222));
        assert_eq!(out.size, Some((1024, 768)));
        assert_eq!(out.ssh_options, vec!["A=1", "B=2"]);
        assert_eq!(out.identity, Some(PathBuf::from("/k/id")));
    }

    #[test]
    fn collect_reports_the_first_problem() {
        let mut e = editor_with(Profile::new("w"));
        // No host yet.
        assert!(e.collect().unwrap_err().contains("host"));
        e.profile.host = "h".into();
        e.ssh_port = "abc".into();
        assert!(e.collect().unwrap_err().contains("SSH port"));
        e.ssh_port.clear();
        e.size = "wide".into();
        assert!(e.collect().unwrap_err().contains("1920x1080"));
    }

    #[test]
    fn blank_option_lines_are_dropped_rather_than_refused() {
        // Typing a trailing newline in the box is normal and must not be an
        // error, even though a blank option would be.
        let mut p = Profile::new("w");
        p.host = "h".into();
        let mut e = editor_with(p);
        e.ssh_options = "A=1\n\n\n".into();
        assert_eq!(e.collect().unwrap().ssh_options, vec!["A=1"]);
    }

    // ---- the launcher's own state -------------------------------------
    //
    // Launcher is a plain struct until eframe hands it a Context, so
    // everything that decides whether the connections file gets written can
    // be exercised without a display.

    /// A launcher pointed at `path`, plus the directory keeping it alive.
    fn launcher_on(text: Option<&str>) -> (tempfile::TempDir, Launcher) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(profiles::FILE_NAME);
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        let launcher = Launcher::new(path);
        (dir, launcher)
    }

    fn named(name: &str, host: &str) -> Profile {
        let mut p = Profile::new(name);
        p.host = host.into();
        p
    }

    const BROKEN: &str = "this is not toml at all";

    #[test]
    fn an_unreadable_file_puts_the_launcher_in_a_read_only_state() {
        let (_dir, launcher) = launcher_on(Some(BROKEN));
        assert!(launcher.load_failed);
        assert!(launcher.error.is_some());
        assert!(launcher.store.items.is_empty());
    }

    #[test]
    fn a_readable_file_does_not() {
        let (_dir, launcher) = launcher_on(Some("[[connection]]\nname = \"a\"\nhost = \"h\"\n"));
        assert!(!launcher.load_failed);
        assert_eq!(launcher.store.items.len(), 1);
        // A missing file is the state before the first save, not a failure.
        let (_dir, launcher) = launcher_on(None);
        assert!(!launcher.load_failed);
    }

    #[test]
    fn a_save_over_an_unreadable_file_is_refused() {
        // The bug this guards: New -> Save replacing a file full of
        // connections with the single one just typed.
        let (_dir, mut launcher) = launcher_on(Some(BROKEN));
        launcher.store.upsert(named("new one", "h"));
        launcher.status = "Saved new one".into();
        launcher.save();
        assert_eq!(
            std::fs::read_to_string(&launcher.path).unwrap(),
            BROKEN,
            "the unreadable file was overwritten"
        );
        // And it must not still be claiming to have saved.
        assert!(launcher.status.is_empty());
        assert!(launcher.error.unwrap().contains("Not saving"));
    }

    #[test]
    fn everything_that_would_write_the_file_is_off_while_it_cannot_be_read() {
        // The gate that matters is the disabled control, not the refusal in
        // `save`: by the time `save` refuses, the status line has already
        // claimed the connection was saved. Every menu item that reaches a
        // write goes through the same check.
        let (_dir, launcher) = launcher_on(Some(BROKEN));
        for action in [
            Action::New,
            Action::Duplicate,
            Action::AskDelete,
            Action::AskImport,
            Action::AskExport,
        ] {
            assert!(!launcher.enabled(&action), "{action:?} should be off");
        }
        // And the one way out of the state is on.
        assert!(launcher.enabled(&Action::MoveAside));
    }

    #[test]
    fn moving_the_bad_file_aside_makes_the_launcher_writable_again() {
        let (dir, mut launcher) = launcher_on(Some(BROKEN));
        launcher.move_bad_file_aside();
        assert!(!launcher.load_failed);
        assert!(launcher.error.is_none());
        assert!(!launcher.path.exists());
        // The connections we could not read are kept, not deleted: the user
        // may want to pick their hosts back out of them.
        let aside = dir.path().join("connections.toml.bad");
        assert_eq!(std::fs::read_to_string(aside).unwrap(), BROKEN);

        launcher.store.upsert(named("new one", "h"));
        launcher.save();
        assert!(launcher.error.is_none(), "{:?}", launcher.error);
        assert_eq!(Profiles::load(&launcher.path).unwrap(), launcher.store);
    }

    /// Put `launcher` in the editor, as though `original` had been opened.
    fn editing(launcher: &mut Launcher, profile: Profile, original: Option<&str>) {
        launcher.view = View::Edit(Box::new(Editor::new(profile, original.map(str::to_string))));
    }

    #[test]
    fn renaming_onto_another_connection_is_refused_rather_than_merging() {
        let (_dir, mut launcher) = launcher_on(None);
        launcher.store.upsert(named("work", "work.example"));
        launcher.store.upsert(named("home", "home.example"));

        let mut renamed = named("home", "work.example");
        renamed.user = "alice".into();
        editing(&mut launcher, renamed, Some("work"));
        launcher.save_editor();

        assert!(launcher.error.unwrap().contains("already"));
        // Both survive, with their own hosts, and the editor stays open so
        // the name can be corrected.
        assert_eq!(launcher.store.items.len(), 2);
        assert_eq!(launcher.store.items[1].host, "home.example");
        assert!(matches!(launcher.view, View::Edit(_)));
        assert!(!launcher.status.contains("Saved"));
    }

    #[test]
    fn a_refusal_is_shown_beside_the_form_as_well_as_on_the_status_line() {
        // A message about the field you are looking at should not live only
        // at the far edge of the window.
        let (_dir, mut launcher) = launcher_on(None);
        launcher.store.upsert(named("work", "work.example"));
        editing(&mut launcher, named("work", "elsewhere"), None);
        launcher.save_editor();
        let View::Edit(editor) = &launcher.view else {
            panic!("the editor closed on a refusal");
        };
        assert_eq!(editor.problem.as_deref(), launcher.error.as_deref());
    }

    #[test]
    fn a_new_connection_may_not_take_a_name_in_use() {
        let (_dir, mut launcher) = launcher_on(None);
        launcher.store.upsert(named("work", "work.example"));
        editing(&mut launcher, named("work", "somewhere.else"), None);
        launcher.save_editor();
        assert!(launcher.error.is_some());
        assert_eq!(launcher.store.items.len(), 1);
        assert_eq!(launcher.store.items[0].host, "work.example");
    }

    #[test]
    fn saving_a_connection_under_its_own_name_still_works() {
        // The exclusion the check needs: an edit that leaves the name alone
        // is the common case and must not be mistaken for a clash.
        let (_dir, mut launcher) = launcher_on(None);
        launcher.store.upsert(named("work", "old.example"));
        editing(&mut launcher, named("work", "new.example"), Some("work"));
        launcher.save_editor();
        assert!(launcher.error.is_none(), "{:?}", launcher.error);
        assert_eq!(launcher.store.items.len(), 1);
        assert_eq!(launcher.store.items[0].host, "new.example");
        assert_eq!(launcher.status, "Saved work");
    }

    #[test]
    fn a_rename_to_a_free_name_still_moves_the_entry() {
        let (_dir, mut launcher) = launcher_on(None);
        launcher.store.upsert(named("work", "work.example"));
        editing(&mut launcher, named("office", "work.example"), Some("work"));
        launcher.save_editor();
        assert!(launcher.error.is_none(), "{:?}", launcher.error);
        let names: Vec<_> = launcher.store.items.iter().map(|p| &p.name).collect();
        assert_eq!(names, vec!["office"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_session_that_exits_badly_is_reported() {
        // /bin/sh stands in for the session binary. Handed a profile's
        // arguments it tries to run "example.org" as a script, fails, and
        // complains on stderr -- the shape of a session that could not
        // connect, which used to be indistinguishable from a click that did
        // nothing at all. The exact code and wording are the shell's, so
        // only the launcher's own framing is asserted here; launch.rs pins
        // down the status text.
        let (_dir, mut launcher) = launcher_on(None);
        launcher
            .sessions
            .start_with(&PathBuf::from("/bin/sh"), &named("work", "example.org"))
            .unwrap();
        for _ in 0..200 {
            launcher.sessions.reap();
            launcher.drain_failures();
            if launcher.error.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let error = launcher.error.expect("a failed session was not reported");
        assert!(error.contains("work (example.org)"), "{error}");
        assert!(error.contains("ended"), "{error}");
        assert!(launcher.status.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn several_sessions_failing_at_once_are_all_accounted_for() {
        // One assignment per failure would leave only the last on the
        // status line and lose the rest silently, which is the ordinary
        // case rather than a strange one: whatever broke the connection
        // usually broke every connection.
        let (_dir, mut launcher) = launcher_on(None);
        for name in ["work", "lab"] {
            launcher
                .sessions
                .start_with(&PathBuf::from("/bin/sh"), &named(name, "example.org"))
                .unwrap();
        }
        for _ in 0..200 {
            launcher.sessions.reap();
            if launcher.sessions.count() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        launcher.drain_failures();
        let error = launcher.error.expect("nothing was reported");
        // One of them in full and the other counted. Which is which is left
        // open: two shells started together do not exit in a fixed order.
        assert!(error.contains("(example.org) ended"), "{error}");
        assert!(error.contains("one other session"), "{error}");
    }

    // ---- filter, keyboard and the new menu actions ---------------------

    /// A launcher holding three connections, nothing else touched.
    fn stocked() -> (tempfile::TempDir, Launcher) {
        let (dir, mut launcher) = launcher_on(None);
        for (name, host) in [
            ("work", "work.example"),
            ("lab gpu", "gpu-01.lan"),
            ("home", "home.example"),
        ] {
            launcher.store.upsert(named(name, host));
        }
        launcher.selected = Some(0);
        (dir, launcher)
    }

    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        ctx
    }

    #[test]
    fn the_filter_matches_the_name_and_the_address() {
        let (_dir, mut launcher) = stocked();
        launcher.filter = "gpu".into();
        // Once by name, once by host -- and only the one row either way.
        assert_eq!(launcher.visible(), vec![1]);
        launcher.filter = "EXAMPLE".into();
        assert_eq!(launcher.visible(), vec![0, 2]);
        launcher.filter = "  ".into();
        assert_eq!(launcher.visible(), vec![0, 1, 2]);
        launcher.filter = "nothing at all".into();
        assert!(launcher.visible().is_empty());
    }

    #[test]
    fn a_selection_the_filter_hides_cannot_be_acted_on() {
        // Filtering does not clear the selection -- typing four characters
        // should not lose the host you had picked -- but Connect must not
        // start something that is not on the screen.
        let (_dir, mut launcher) = stocked();
        launcher.selected = Some(0);
        launcher.filter = "gpu".into();
        assert!(!launcher.selection_is_visible());
        assert!(!launcher.enabled(&Action::Connect));
        assert!(!launcher.enabled(&Action::AskDelete));
        launcher.filter.clear();
        assert!(launcher.enabled(&Action::Connect));
    }

    #[test]
    fn the_arrows_walk_the_rows_the_filter_is_showing() {
        let (_dir, mut launcher) = stocked();
        launcher.move_selection(1);
        assert_eq!(launcher.selected, Some(1));
        launcher.move_selection(1);
        assert_eq!(launcher.selected, Some(2));
        // Clamped at the ends rather than wrapping: a list that jumps from
        // the bottom back to the top loses you your place.
        launcher.move_selection(1);
        assert_eq!(launcher.selected, Some(2));
        launcher.move_selection(-5);
        assert_eq!(launcher.selected, Some(0));

        // And the hidden rows are skipped, not walked through invisibly.
        launcher.filter = "example".into();
        launcher.selected = Some(0);
        launcher.move_selection(1);
        assert_eq!(
            launcher.selected,
            Some(2),
            "stepped onto a filtered-out row"
        );
    }

    #[test]
    fn moving_from_a_hidden_selection_lands_on_an_end() {
        let (_dir, mut launcher) = stocked();
        launcher.filter = "example".into();
        launcher.selected = Some(1); // filtered out
        launcher.move_selection(1);
        assert_eq!(launcher.selected, Some(0));
        launcher.selected = Some(1);
        launcher.move_selection(-1);
        assert_eq!(launcher.selected, Some(2));
    }

    #[test]
    fn duplicate_copies_everything_but_the_name_and_saves() {
        let (_dir, mut launcher) = stocked();
        let mut source = named("work", "work.example");
        source.user = "alice".into();
        source.ssh_port = Some(2222);
        source.identity = Some(PathBuf::from("/k/id"));
        launcher.store.upsert(source.clone());
        launcher.selected = launcher.store.position("work");

        launcher.perform(Action::Duplicate, &ctx());
        let copy = launcher
            .store
            .items
            .iter()
            .find(|p| p.name == "work copy")
            .expect("the duplicate was not added");
        assert_eq!(copy.host, source.host);
        assert_eq!(copy.user, source.user);
        assert_eq!(copy.ssh_port, source.ssh_port);
        assert_eq!(copy.identity, source.identity);
        // Selected, so the obvious next move -- Edit -- lands on the copy and
        // not on the original.
        assert_eq!(launcher.selected, launcher.store.position("work copy"));
        // And on disk, not merely in the window.
        assert!(Profiles::load(&launcher.path)
            .unwrap()
            .position("work copy")
            .is_some());

        // Twice does not collide, because the name is the key.
        launcher.selected = launcher.store.position("work");
        launcher.perform(Action::Duplicate, &ctx());
        assert!(launcher.store.position("work copy 2").is_some());
    }

    #[test]
    fn an_import_renames_a_clash_rather_than_replacing_the_host_behind_it() {
        let (dir, mut launcher) = stocked();
        let other = dir.path().join("other.toml");
        std::fs::write(
            &other,
            "[[connection]]\nname = \"work\"\nhost = \"somewhere.else\"\n\
             [[connection]]\nname = \"fresh\"\nhost = \"new.example\"\n",
        )
        .unwrap();

        launcher.import_from(&other);
        assert!(launcher.error.is_none(), "{:?}", launcher.error);
        assert_eq!(launcher.status, "Imported 2 connections (1 renamed)");
        // The original "work" is untouched; the incoming one arrived beside
        // it under a free name.
        assert_eq!(launcher.store.items[0].host, "work.example");
        assert_eq!(
            launcher.store.items[launcher.store.position("work 2").unwrap()].host,
            "somewhere.else"
        );
        assert!(launcher.store.position("fresh").is_some());
        // Saved, not only shown.
        assert_eq!(Profiles::load(&launcher.path).unwrap(), launcher.store);
    }

    #[test]
    fn importing_the_open_file_is_refused_rather_than_duplicating_everything() {
        // Every name would collide, every one would be renamed to a copy, and
        // the doubled list would then be saved over the original.
        let (dir, mut launcher) = stocked();
        launcher.save();
        launcher.import_from(&launcher.path.clone());
        assert_eq!(launcher.store.items.len(), 3);
        assert!(launcher.error.take().unwrap().contains("already showing"));
        // Including by a path that only spells it differently.
        let roundabout = dir.path().join("sub").join("..").join(profiles::FILE_NAME);
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        launcher.import_from(&roundabout);
        assert_eq!(launcher.store.items.len(), 3);
        assert!(launcher.error.is_some());
    }

    #[test]
    fn an_import_that_would_not_fit_is_refused_whole() {
        // Half an import is worse than none: the user cannot tell which half.
        let (dir, mut launcher) = stocked();
        let other = dir.path().join("many.toml");
        let mut text = String::new();
        for i in 0..profiles::MAX_PROFILES {
            text.push_str(&format!("[[connection]]\nname = \"n{i}\"\nhost = \"h\"\n"));
        }
        std::fs::write(&other, text).unwrap();

        launcher.import_from(&other);
        assert_eq!(launcher.store.items.len(), 3, "some of it went in anyway");
        let error = launcher.error.expect("nothing was said");
        assert!(
            error.contains(&profiles::MAX_PROFILES.to_string()),
            "{error}"
        );
        assert!(error.contains("Nothing was imported"), "{error}");
    }

    #[test]
    fn a_missing_or_broken_import_says_so_and_changes_nothing() {
        let (dir, mut launcher) = stocked();
        launcher.import_from(&dir.path().join("nowhere.toml"));
        assert!(launcher.error.take().is_some());
        assert_eq!(launcher.store.items.len(), 3);

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, BROKEN).unwrap();
        launcher.import_from(&bad);
        assert!(launcher.error.is_some());
        assert_eq!(launcher.store.items.len(), 3);
    }

    #[test]
    fn an_export_writes_a_file_the_launcher_can_read_back() {
        let (dir, mut launcher) = stocked();
        let out = dir.path().join("export.toml");
        launcher.export_to(&out);
        assert!(launcher.error.is_none(), "{:?}", launcher.error);
        assert!(
            launcher.status.contains("3 connections"),
            "{}",
            launcher.status
        );
        assert_eq!(Profiles::load(&out).unwrap(), launcher.store);
        // Nothing secret went with it, because there is nothing secret to go.
        let text = std::fs::read_to_string(&out).unwrap().to_lowercase();
        assert!(!text.contains("password"), "{text}");
        assert!(!text.contains("passphrase"), "{text}");
    }

    #[test]
    fn reloading_picks_up_a_hand_edit_and_keeps_the_selection() {
        // The README tells people to edit the file by hand, so the window has
        // to have a way to notice.
        let (_dir, mut launcher) = stocked();
        launcher.save();
        launcher.selected = Some(2);
        let mut on_disk = launcher.store.clone();
        on_disk.upsert(named("added by hand", "typed.example"));
        on_disk.save(&launcher.path).unwrap();

        launcher.reload();
        assert_eq!(launcher.store.items.len(), 4);
        assert_eq!(launcher.selected_name().as_deref(), Some("home"));
        assert_eq!(launcher.status, "Reloaded 4 connections");
    }

    #[test]
    fn reloading_a_file_that_has_since_broken_stops_the_writes() {
        let (_dir, mut launcher) = stocked();
        std::fs::write(&launcher.path, BROKEN).unwrap();
        launcher.reload();
        assert!(launcher.load_failed);
        assert!(launcher.error.is_some());
        assert!(launcher.store.items.is_empty());
        // And the way back is still open once the file is repaired.
        launcher.store = Profiles::default();
        std::fs::write(&launcher.path, "").unwrap();
        launcher.reload();
        assert!(!launcher.load_failed);
    }

    #[test]
    fn the_theme_choice_survives_a_restart() {
        // A view preference, not a credential, so it is saved -- but in its
        // own file: a new key in connections.toml would make every older
        // client refuse to parse it, which is the read-only state.
        let (dir, mut launcher) = stocked();
        launcher.perform(Action::SetTheme(ThemeChoice::Light), &ctx());
        launcher.perform(Action::ToggleCompactRows, &ctx());
        launcher.perform(Action::Zoom(1.4), &ctx());

        let again = Launcher::new(dir.path().join(profiles::FILE_NAME));
        assert_eq!(again.settings.theme, ThemeChoice::Light);
        assert!(again.settings.compact_rows);
        assert!((again.settings.zoom - 1.4).abs() < 0.001);
        // And it did not go into the connections file, which an older client
        // would then refuse.
        let connections =
            std::fs::read_to_string(dir.path().join(profiles::FILE_NAME)).unwrap_or_default();
        assert!(!connections.contains("theme"), "{connections}");
    }

    #[test]
    fn zoom_stays_inside_the_range_the_window_was_designed_for() {
        let (_dir, mut launcher) = stocked();
        launcher.perform(Action::Zoom(9.0), &ctx());
        assert_eq!(launcher.settings.zoom, theme::ZOOM_MAX);
        launcher.perform(Action::Zoom(0.0), &ctx());
        assert_eq!(launcher.settings.zoom, theme::ZOOM_MIN);
    }

    #[test]
    fn unreadable_view_settings_fall_back_rather_than_stopping_anything() {
        // The opposite of the connections file, on purpose: losing a theme
        // choice is a shrug, and a launcher that refused to save connections
        // because its window preferences were malformed would be absurd.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::settings::FILE_NAME), BROKEN).unwrap();
        let launcher = Launcher::new(dir.path().join(profiles::FILE_NAME));
        assert!(!launcher.load_failed);
        assert_eq!(launcher.settings, Settings::default());
        assert!(launcher.status.contains("default view settings"));
    }

    #[test]
    fn the_copied_command_line_is_the_one_the_launcher_would_run() {
        // The point of the whole re-invoke-ourselves design: what the GUI
        // does and what the command line does cannot drift. If this ever
        // needs a special case, the two have drifted.
        let (_dir, mut launcher) = launcher_on(None);
        let mut p = named("work", "work.example");
        p.user = "alice".into();
        p.ssh_port = Some(2222);
        p.ssh_options = vec!["ProxyJump=bastion".into()];
        launcher.store.upsert(p.clone());
        launcher.selected = Some(0);
        launcher.perform(Action::CopyCommandLine, &ctx());
        let copied = launcher.status.trim_start_matches("Copied: ").to_string();
        assert_eq!(copied, format!("lynxrdp {}", shell_join(&p.args())));
        assert!(copied.starts_with("lynxrdp alice@work.example --port 2222"));
    }

    #[test]
    fn the_command_line_preview_quotes_but_never_executes() {
        // Display only. The launcher spawns argv directly; this exists so a
        // user can see the equivalence, and it must not make a value that
        // looks safe when it is not.
        assert_eq!(shell_join(&["a".into(), "b".into()]), "a b");
        assert_eq!(shell_join(&["a b".into()]), "'a b'");
        assert_eq!(shell_join(&["it's".into()]), r"'it'\''s'");
        assert_eq!(shell_join(&["-o".into(), "X=1".into()]), "-o X=1");
        assert_eq!(shell_join(&["".into()]), "''");
        assert_eq!(shell_join(&["$(rm -rf /)".into()]), "'$(rm -rf /)'");
    }

    #[test]
    fn every_shortcut_is_listed_where_a_user_can_find_it() {
        // The arrows, `/` and Escape are the ones that cannot be discovered
        // by opening a menu, so Help must carry them.
        // Inside a frame: `format_shortcut` asks the fonts whether they have
        // a glyph for the command symbol, and there are none before the
        // first pass.
        let ctx = ctx();
        let mut listed = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| listed = shortcut_list(ctx));
        let labels: Vec<&str> = listed.iter().map(|(label, _)| *label).collect();
        for expected in [
            "Connect",
            "Move through the list",
            "Filter the list",
            "Clear the filter",
            "Delete",
        ] {
            assert!(labels.contains(&expected), "{expected} is not listed");
        }
        assert!(listed.iter().all(|(_, keys)| !keys.is_empty()));
    }

    // ---- layout --------------------------------------------------------
    //
    // egui needs no window, only eframe does, so a whole frame of the real
    // window can be laid out here and measured. `draw` is what `update`
    // calls, so these measure what ships rather than a reconstruction.

    /// Width used for the laid-out status bar. Much narrower than the
    /// launcher's real window, so that a realistic SSH complaint has to wrap
    /// rather than happening to fit.
    const BAR_WIDTH: f32 = 300.0;

    /// The launcher's own starting size, from `run` above.
    const WINDOW: [f32; 2] = [880.0, 560.0];

    /// One piece of text a frame drew.
    ///
    /// `elided` comes from the galley rather than from the string: an
    /// ellipsised galley still reports its *full* text and only its rectangle
    /// shrinks, so "did this get cut off" cannot be asked of the characters.
    struct Text {
        text: String,
        rect: egui::Rect,
        elided: bool,
    }

    /// Everything one frame drew, in screen coordinates.
    #[derive(Default)]
    struct Painted {
        texts: Vec<Text>,
        rects: Vec<(egui::Rect, egui::Color32, egui::Stroke)>,
        lines: Vec<[egui::Pos2; 2]>,
    }

    impl Painted {
        /// The one drawn text that is `needle`, or the one that contains it.
        ///
        /// Exact first, because the window is full of strings that contain
        /// each other: "Connect" is in "Connection", "work" is in
        /// "work.example".
        fn find(&self, needle: &str) -> &Text {
            let exact: Vec<&Text> = self.texts.iter().filter(|t| t.text == needle).collect();
            if exact.len() == 1 {
                return exact[0];
            }
            let mut hits = self.texts.iter().filter(|t| t.text.contains(needle));
            let found = hits.next().unwrap_or_else(|| {
                let all: Vec<&String> = self.texts.iter().map(|t| &t.text).collect();
                panic!("{needle:?} was not drawn; the frame had {all:?}")
            });
            assert!(hits.next().is_none(), "{needle:?} was drawn more than once");
            found
        }

        fn text(&self, needle: &str) -> egui::Rect {
            self.find(needle).rect
        }

        fn elided(&self, needle: &str) -> bool {
            self.find(needle).elided
        }

        fn has_text(&self, needle: &str) -> bool {
            self.texts.iter().any(|t| t.text.contains(needle))
        }

        /// Every rectangle filled with `colour`.
        fn filled(&self, colour: egui::Color32) -> Vec<egui::Rect> {
            self.rects
                .iter()
                .filter(|(_, fill, _)| *fill == colour)
                .map(|(rect, _, _)| *rect)
                .collect()
        }

        /// Every rectangle outlined `width` points wide in `colour`.
        fn stroked(&self, width: f32, colour: egui::Color32) -> Vec<egui::Rect> {
            self.rects
                .iter()
                .filter(|(_, _, stroke)| stroke.width == width && stroke.color == colour)
                .map(|(rect, _, _)| *rect)
                .collect()
        }
    }

    fn collect(shape: &egui::Shape, out: &mut Painted) {
        match shape {
            // The galley's own rect already accounts for its alignment,
            // which a rect built from the position and the size would not:
            // a right-aligned galley is laid out to the left of its
            // position, not to the right.
            egui::Shape::Text(text) => out.texts.push(Text {
                text: text.galley.text().to_string(),
                rect: text.galley.rect.translate(text.pos.to_vec2()),
                elided: text.galley.elided,
            }),
            egui::Shape::Rect(rect) => out.rects.push((rect.rect, rect.fill, rect.stroke)),
            egui::Shape::LineSegment { points, .. } => out.lines.push(*points),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect(shape, out);
                }
            }
            _ => {}
        }
    }

    /// Lay the whole window out and report what it painted.
    ///
    /// Three passes: a panel's height is not known until its contents have
    /// been laid out once, and a Grid's column widths settle on the second,
    /// so only the last frame is worth measuring.
    fn frames(
        launcher: &mut Launcher,
        size: [f32; 2],
        prepare: impl Fn(&egui::Context),
    ) -> Painted {
        let ctx = ctx();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size[0], size[1]),
            )),
            ..Default::default()
        };
        let mut output = None;
        for _ in 0..3 {
            prepare(&ctx);
            output = Some(ctx.run(input(), |ctx| launcher.draw(ctx)));
        }
        let mut painted = Painted::default();
        for clipped in &output.unwrap().shapes {
            collect(&clipped.shape, &mut painted);
        }
        painted
    }

    fn window(launcher: &mut Launcher) -> Painted {
        frames(launcher, WINDOW, |_| {})
    }

    /// Open one top-level menu with the pointer and report what it drew.
    ///
    /// Clicked rather than poked into egui's memory, because what is worth
    /// checking is that the items are there *and* reachable: a menu whose
    /// button no longer opens it would pass any test that built the popup
    /// directly.
    fn menu(launcher: &mut Launcher, title: &str) -> Painted {
        let ctx = ctx();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WINDOW[0], WINDOW[1]));
        let run = |launcher: &mut Launcher, events: Vec<egui::Event>| {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| launcher.draw(ctx),
            );
            let mut painted = Painted::default();
            for clipped in &output.shapes {
                collect(&clipped.shape, &mut painted);
            }
            painted
        };
        run(launcher, Vec::new());
        let at = run(launcher, Vec::new()).text(title).center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run(
            launcher,
            vec![egui::Event::PointerMoved(at), button(true), button(false)],
        );
        run(launcher, vec![egui::Event::PointerMoved(at)])
    }

    #[test]
    fn every_menu_lists_what_it_promises() {
        // Each of these is backed by something that exists; there are no
        // greyed-out placeholders for features that were never written.
        let (_dir, mut launcher) = stocked();
        for (title, items) in [
            (
                "File",
                &[
                    "New Connection",
                    "Duplicate",
                    "Delete",
                    "Import Connections",
                    "Export Connections",
                    "Quit",
                ][..],
            ),
            (
                "Connection",
                &[
                    "Connect",
                    "Edit",
                    "Copy Command Line",
                    "Copy Connections File Path",
                    "Reload From Disk",
                    "Move Broken File Aside",
                ][..],
            ),
            (
                "View",
                &[
                    "Theme",
                    "Compact Rows",
                    "Show Command Line",
                    "Zoom In",
                    "Zoom Out",
                    "Actual Size",
                ][..],
            ),
            (
                "Help",
                &["Keyboard Shortcuts", "Documentation", "About LynxRDP"][..],
            ),
        ] {
            let painted = menu(&mut launcher, title);
            for item in items {
                assert!(
                    painted.has_text(item),
                    "the {title} menu is missing {item:?}"
                );
            }
        }
    }

    #[test]
    fn a_menu_item_prints_the_key_that_does_the_same_thing() {
        // The menu is where a shortcut is discovered, so an item whose
        // accelerator went missing is a feature nobody finds.
        let (_dir, mut launcher) = stocked();
        let ctx = ctx();
        let mut expected = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            expected = ctx.format_shortcut(&keys::NEW);
        });
        let painted = menu(&mut launcher, "File");
        assert!(
            painted.has_text(&expected),
            "New Connection does not show {expected:?}"
        );
    }

    #[test]
    fn the_menu_bar_is_one_control_tall_and_carries_every_menu() {
        let (_dir, mut launcher) = stocked();
        let painted = window(&mut launcher);
        for menu in ["File", "Connection", "View", "Help"] {
            let rect = painted.text(menu);
            assert!(
                rect.top() >= 0.0 && rect.bottom() <= theme::MENU_BAR_HEIGHT,
                "{menu} at {rect:?} is outside the {} point bar",
                theme::MENU_BAR_HEIGHT
            );
        }
        // The bar's own height, measured from the hairline it draws along its
        // bottom edge rather than asserted from the constant.
        let seam = painted
            .lines
            .iter()
            .map(|[a, _]| a.y)
            .find(|y| (*y - (theme::MENU_BAR_HEIGHT - 0.5)).abs() < 0.6)
            .unwrap_or_else(|| panic!("no seam under the menu bar: {:?}", painted.lines));
        assert!((seam + 0.5 - theme::MENU_BAR_HEIGHT).abs() < 0.6);
    }

    #[test]
    fn a_row_is_one_height_and_the_selected_one_carries_an_edge_as_well_as_a_fill() {
        let (_dir, mut launcher) = stocked();
        launcher.selected = Some(1);
        let painted = window(&mut launcher);
        let t = theme::tokens(true);

        let selected = painted.filled(t.accent_weak);
        assert_eq!(selected.len(), 1, "one row is selected, so one fill");
        let row = selected[0];
        assert_eq!(row.height(), theme::ROW_HEIGHT);
        assert_eq!(row.left(), theme::LIST_MARGIN);
        assert_eq!(row.right(), WINDOW[0] - theme::LIST_MARGIN);

        // Colour alone is 1.34:1 against the surface, so selection is also an
        // edge. It has to be inside the row it belongs to.
        let edges = painted.filled(t.accent);
        let edge = edges
            .iter()
            .find(|e| row.contains_rect(**e))
            .unwrap_or_else(|| panic!("no accent edge inside {row:?}: {edges:?}"));
        assert_eq!(edge.width(), theme::SELECTED_EDGE);
        assert_eq!(edge.height(), theme::ROW_HEIGHT);
        assert_eq!(edge.left(), row.left());
    }

    #[test]
    fn rows_are_evenly_spaced_and_their_text_lines_up() {
        let (_dir, mut launcher) = stocked();
        let painted = window(&mut launcher);
        let (work, lab, home) = (
            painted.text("work"),
            painted.text("lab gpu"),
            painted.text("home"),
        );
        let step = theme::ROW_HEIGHT + theme::ROW_GAP;
        assert!(
            (lab.top() - work.top() - step).abs() < 0.6,
            "{work:?} {lab:?}"
        );
        assert!((home.top() - lab.top() - step).abs() < 0.6);
        // One left edge down the whole list, selected or not.
        for rect in [work, lab, home] {
            assert!(
                (rect.left() - theme::ROW_TEXT_INSET).abs() < 0.6,
                "{rect:?} is not on the list's left edge"
            );
        }
        // The address is under the name, in the same block.
        let address = painted.text("gpu-01.lan");
        assert!(address.top() >= lab.bottom() - 1.0, "{lab:?} {address:?}");
        assert!(address.bottom() <= lab.top() + theme::ROW_HEIGHT);
    }

    #[test]
    fn compact_rows_are_shorter_and_keep_the_address_whole() {
        // The address is what identifies the host, so when the two collide it
        // is the name that gets the ellipsis.
        const NARROW: [f32; 2] = [300.0, 400.0];
        let long = "a connection with a very long name indeed";

        let (_dir, mut launcher) = stocked();
        launcher.settings.compact_rows = true;
        launcher.selected = Some(0);
        // The same row twice, so the address can be compared with itself
        // rather than with a number typed into the test.
        let unsqueezed = frames(&mut launcher, NARROW, |_| {})
            .text("work.example")
            .width();

        launcher.store.items[0].name = long.into();
        let painted = frames(&mut launcher, NARROW, |_| {});

        let row = painted.filled(theme::tokens(true).accent_weak)[0];
        assert_eq!(row.height(), theme::ROW_HEIGHT_COMPACT);

        let address = painted.text("work.example");
        assert!(!painted.elided("work.example"), "the address was cut off");
        assert_eq!(
            address.width(),
            unsqueezed,
            "the address gave way to the name"
        );
        assert!(
            address.right() <= NARROW[0] - theme::LIST_MARGIN + 0.6,
            "{address:?}"
        );

        let name = painted.text(long);
        assert!(painted.elided(long), "the long name was not ellipsised");
        assert!(
            name.right() <= address.left(),
            "{name:?} runs into {address:?}"
        );
    }

    #[test]
    fn a_focused_row_paints_a_ring() {
        // A raw-painted row is not a widget egui will outline for us, and
        // Tab already reaches it: without this the keyboard path is invisible.
        let (_dir, mut launcher) = stocked();
        let painted = frames(&mut launcher, WINDOW, |ctx| {
            ctx.memory_mut(|m| m.request_focus(row_id(1)));
        });
        let t = theme::tokens(true);
        let rings = painted.stroked(theme::FOCUS_RING, theme::focus_ring(&t, true));
        assert_eq!(rings.len(), 1, "expected one focus ring, got {rings:?}");
        assert!((rings[0].height() - (theme::ROW_HEIGHT - 2.0)).abs() < 0.6);
    }

    #[test]
    fn the_primary_button_is_a_full_control_tall() {
        // A bare TextEdit measures about 15 points and a stock button a
        // little under 20; the bar has to be one height throughout or it
        // reads as unfinished.
        let (_dir, mut launcher) = stocked();
        let painted = window(&mut launcher);
        let t = theme::tokens(true);
        let primaries = painted.filled(t.accent);
        assert!(
            primaries
                .iter()
                .any(|r| (r.height() - theme::CONTROL_HEIGHT).abs() < 0.6),
            "no {}-point primary button among {primaries:?}",
            theme::CONTROL_HEIGHT
        );
        // Connect is the primary action and sits at the left of the bar.
        let connect = painted.text("Connect");
        assert!(connect.left() < WINDOW[0] / 3.0, "{connect:?}");
        assert!(connect.top() > WINDOW[1] - theme::ACTION_BAR_HEIGHT * 2.0);
    }

    #[test]
    fn the_command_line_strip_appears_only_when_it_is_asked_for() {
        let (_dir, mut launcher) = stocked();
        launcher.selected = Some(0);
        assert!(!window(&mut launcher).has_text("lynxrdp work.example"));

        launcher.settings.show_command_line = true;
        let painted = window(&mut launcher);
        let strip = painted.text("lynxrdp work.example");
        // Between the list and the action bar, and inside the window.
        assert!(strip.right() <= WINDOW[0]);
        assert!(strip.top() > WINDOW[1] / 2.0, "{strip:?}");
    }

    #[test]
    fn a_long_modal_keeps_its_own_close_button_on_screen() {
        // The shortcut list is taller than the smallest window, and a modal
        // that ran off both ends would take its only way out with it.
        let (_dir, mut launcher) = stocked();
        launcher.perform(Action::ShowShortcuts, &ctx());
        let painted = frames(&mut launcher, [640.0, 420.0], |_| {});
        let heading = painted.text("Keyboard shortcuts");
        let close = painted.text("Close");
        assert!(heading.top() >= 0.0, "{heading:?}");
        assert!(
            close.bottom() <= 420.0,
            "Close ran off the bottom: {close:?}"
        );
    }

    #[test]
    fn cancelling_a_delete_beats_the_key_that_confirms_one() {
        // egui turns Enter on a focused widget into a click, so a user who
        // tabs to Cancel and presses Enter produces a Cancel click *and* the
        // bare Enter that means "confirm" in the same frame. Acting on the
        // second would delete the connection they had just decided to keep.
        // Clicking Cancel with Enter in the same frame is that state exactly.
        let (_dir, mut launcher) = stocked();
        launcher.perform(Action::AskDelete, &ctx());
        let ctx = ctx();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(WINDOW[0], WINDOW[1]));
        let run = |launcher: &mut Launcher, events: Vec<egui::Event>| {
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(screen),
                    events,
                    ..Default::default()
                },
                |ctx| launcher.draw(ctx),
            );
            let mut painted = Painted::default();
            for clipped in &output.shapes {
                collect(&clipped.shape, &mut painted);
            }
            painted
        };
        run(&mut launcher, Vec::new());
        let at = run(&mut launcher, Vec::new()).text("Cancel").center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        run(
            &mut launcher,
            vec![
                egui::Event::PointerMoved(at),
                button(true),
                button(false),
                egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert_eq!(
            launcher.store.items.len(),
            3,
            "Cancel deleted the connection anyway"
        );
        assert!(launcher.dialog.is_none(), "the dialog would not close");
    }

    #[test]
    fn about_says_what_a_saved_connection_does_not_hold() {
        // The one invariant a user of this window has to be able to check.
        let (_dir, mut launcher) = stocked();
        launcher.perform(Action::ShowAbout, &ctx());
        let painted = window(&mut launcher);
        assert!(painted.has_text("never hold a password"));
        assert!(painted.has_text(env!("CARGO_PKG_VERSION")));
        assert!(painted.has_text(&launcher.path.display().to_string()));
    }

    #[test]
    fn a_filter_that_matches_nothing_offers_the_way_out() {
        let (_dir, mut launcher) = stocked();
        launcher.filter = "zzz".into();
        let painted = window(&mut launcher);
        assert!(painted.has_text("Nothing matches"));
        assert!(painted.has_text("Clear the filter"));
        assert!(!painted.has_text("work.example"));
    }

    #[test]
    fn an_unreadable_file_is_explained_once_rather_than_three_times() {
        // Three different reasons the list can be empty -- unreadable, none
        // saved, none matched -- and only one of them is true at a time.
        let (_dir, mut launcher) = launcher_on(Some(BROKEN));
        let painted = window(&mut launcher);
        assert!(painted.has_text("could not be read"));
        assert!(painted.has_text("Move it aside"));
        assert!(!painted.has_text("Nothing matches"));
        assert!(!painted.has_text("No connections yet"));
    }

    #[test]
    fn an_empty_list_says_so_rather_than_blaming_the_filter() {
        let (_dir, mut launcher) = launcher_on(None);
        let painted = window(&mut launcher);
        assert!(painted.has_text("No connections yet"));
        assert!(!painted.has_text("Nothing matches"));
    }

    #[test]
    fn nothing_the_window_draws_runs_off_its_right_edge() {
        // Everything is laid out from tokens rather than from the window, so
        // the narrowest window the user can make must still hold together.
        let (_dir, mut launcher) = stocked();
        launcher.settings.show_command_line = true;
        launcher.store.items[0].name = "a connection with a name far longer than the window".into();
        launcher.store.items[0].host = "an-extremely-long-hostname.some.subdomain.example".into();
        let painted = frames(&mut launcher, [640.0, 420.0], |_| {});
        for drawn in &painted.texts {
            assert!(
                drawn.rect.right() <= 640.0 + 0.6,
                "{:?} runs to {} of 640",
                drawn.text,
                drawn.rect.right()
            );
        }
    }

    /// Run one whole frame with `key` pressed and hand back the context, so a
    /// test can ask where the focus ended up.
    ///
    /// End to end on purpose: the accelerators are consumed before any widget
    /// is built, and a test that called the handler directly would not notice
    /// if the panels stopped being reached.
    fn press(launcher: &mut Launcher, key: egui::Key, ctx: &egui::Context) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(WINDOW[0], WINDOW[1]),
            )),
            events: vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| launcher.draw(ctx));
    }

    #[test]
    fn the_arrow_keys_reach_the_list_through_a_whole_frame() {
        let (_dir, mut launcher) = stocked();
        let ctx = ctx();
        press(&mut launcher, egui::Key::ArrowDown, &ctx);
        assert_eq!(launcher.selected, Some(1));
        press(&mut launcher, egui::Key::ArrowDown, &ctx);
        assert_eq!(launcher.selected, Some(2));
        press(&mut launcher, egui::Key::ArrowUp, &ctx);
        assert_eq!(launcher.selected, Some(1));
    }

    #[test]
    fn slash_puts_the_caret_in_the_filter_box_and_escape_empties_it() {
        let (_dir, mut launcher) = stocked();
        let ctx = ctx();
        press(&mut launcher, egui::Key::Slash, &ctx);
        assert_eq!(
            ctx.memory(|m| m.focused()),
            Some(egui::Id::new(FILTER_ID)),
            "/ did not reach the filter"
        );
        // And while it has the caret, `/` is a character again rather than a
        // jump -- otherwise a host with a slash in an option could never be
        // typed.
        launcher.filter = "gpu".into();
        press(&mut launcher, egui::Key::Escape, &ctx);
        assert!(
            launcher.filter.is_empty(),
            "Escape did not clear the filter"
        );
    }

    /// What the accelerators make of a bare Enter while `focus` holds the
    /// keyboard focus.
    ///
    /// `accelerators` rather than a whole frame on purpose: performing
    /// `Connect` spawns a real child process, and what is under test here is
    /// only which key belongs to whom.
    fn enter_with_focus(launcher: &Launcher, focus: Option<egui::Id>) -> Option<Action> {
        let ctx = ctx();
        if let Some(id) = focus {
            ctx.memory_mut(|m| m.request_focus(id));
        }
        let mut chosen = None;
        let _ = ctx.run(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Enter,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ctx| chosen = launcher.accelerators(ctx),
        );
        chosen
    }

    #[test]
    fn enter_connects_from_the_list_but_belongs_to_a_focused_button() {
        // egui activates a focused widget on Enter as well as on Space, so an
        // Enter taken globally made every button on the window unreachable
        // from the keyboard: Tab to New, press Enter, and the launcher
        // connected to whatever row happened to be selected instead.
        let (_dir, launcher) = stocked();
        assert_eq!(
            enter_with_focus(&launcher, Some(row_id(0))),
            Some(Action::Connect),
            "Enter on the focused row is how you connect"
        );
        assert_eq!(
            enter_with_focus(&launcher, Some(egui::Id::new(FILTER_ID))),
            Some(Action::Connect),
            "type, narrow down, Enter -- the point of the filter box"
        );
        assert_eq!(enter_with_focus(&launcher, None), Some(Action::Connect));
        assert_eq!(
            enter_with_focus(&launcher, Some(egui::Id::new("some-button"))),
            None,
            "Enter was taken from a focused button"
        );
    }

    #[test]
    fn delete_asks_before_it_removes_anything() {
        let (_dir, mut launcher) = stocked();
        let ctx = ctx();
        press(&mut launcher, keys::DELETE.logical_key, &ctx);
        assert!(
            matches!(&launcher.dialog, Some(Dialog::ConfirmDelete(name)) if name == "work"),
            "the key deleted without asking, or did nothing"
        );
        assert_eq!(launcher.store.items.len(), 3);
    }

    #[test]
    fn escape_leaves_the_editor_without_writing_anything() {
        let (_dir, mut launcher) = stocked();
        launcher.perform(Action::Edit, &ctx());
        let ctx = ctx();
        // A frame first, so the editor is on screen the way it would be.
        press(&mut launcher, egui::Key::ArrowRight, &ctx);
        press(&mut launcher, egui::Key::Escape, &ctx);
        assert!(matches!(launcher.view, View::List));
        assert_eq!(launcher.store.items.len(), 3);
        assert!(!launcher.status.contains("Saved"));
    }

    #[test]
    fn the_editor_keeps_save_and_cancel_on_screen_in_the_smallest_window() {
        // The form is taller than the smallest window the user may make, so
        // the two things a half-filled form needs must not be the parts that
        // scroll away.
        let (_dir, mut launcher) = stocked();
        launcher.perform(Action::Edit, &ctx());
        let painted = frames(&mut launcher, [640.0, 420.0], |_| {});
        let save = painted.text("Save");
        let cancel = painted.text("Cancel");
        assert!(save.bottom() <= 420.0, "Save fell off the bottom: {save:?}");
        assert!(cancel.bottom() <= 420.0, "{cancel:?}");
        assert!(save.top() > 420.0 - theme::ACTION_BAR_HEIGHT * 2.0);
        // And the form itself is still there, scrolled.
        assert!(painted.has_text("Host"));
    }

    // ---- the status bar ------------------------------------------------
    //
    // Laid out on its own, in a much narrower window than the real one, so
    // that a realistic SSH complaint has to wrap rather than happening to fit.

    /// Lay out the status bar and return every piece of text that was drawn
    /// with the rectangle it occupies, in screen coordinates.
    fn status_bar(launcher: &mut Launcher) -> Painted {
        status_bar_in(launcher, egui::vec2(BAR_WIDTH, 300.0)).0
    }

    /// The same, in a window of `window` points, also returning how tall the
    /// bar itself ended up.
    fn status_bar_in(launcher: &mut Launcher, window: egui::Vec2) -> (Painted, f32) {
        let ctx = ctx();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, window)),
            ..Default::default()
        };
        let mut height = 0.0;
        let frame = |launcher: &mut Launcher, height: &mut f32| {
            ctx.run(input(), |ctx| {
                let panel =
                    egui::TopBottomPanel::bottom("status").show(ctx, |ui| launcher.status_ui(ui));
                *height = panel.response.rect.height();
            })
        };
        // Twice: a bottom panel's height is not known until its contents
        // have been laid out once, so the first frame puts it in the wrong
        // place and the second is the one worth measuring.
        let _ = frame(launcher, &mut height);
        let output = frame(launcher, &mut height);
        let mut painted = Painted::default();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut painted);
        }
        (painted, height)
    }

    #[test]
    fn the_status_line_reads_from_the_left() {
        // A Label inherits the horizontal placement of the layout it is
        // added to, so one dropped straight into the right-to-left layout
        // that positions the session count comes out right-aligned: a short
        // status would float to the far side of the window, away from the
        // list it is talking about.
        let (_dir, mut launcher) = launcher_on(None);
        launcher.status = "Saved work".into();
        let rect = status_bar(&mut launcher).text("Saved work");
        assert!(rect.left() < BAR_WIDTH / 4.0, "{rect:?}");
    }

    #[test]
    fn a_long_failure_wraps_instead_of_running_off_the_edge() {
        let (_dir, mut launcher) = launcher_on(None);
        launcher.status = "Saved work".into();
        let one_line = status_bar(&mut launcher).text("Saved work").height();

        launcher.status.clear();
        launcher.error = Some(
            "work (alice@10.0.0.5) ended -- exit code 255:\n\
             alice@10.0.0.5: Permission denied (publickey,keyboard-interactive)."
                .into(),
        );
        let rect = status_bar(&mut launcher).text("Permission denied");
        assert!(rect.left() < BAR_WIDTH / 4.0, "{rect:?}");
        assert!(rect.right() <= BAR_WIDTH, "ran off the edge: {rect:?}");
        assert!(rect.height() > one_line * 2.0, "did not wrap: {rect:?}");
    }

    #[test]
    fn the_worst_message_still_leaves_most_of_the_window() {
        // What bounds the tail in `launch` is what keeps this window
        // usable: the bar sizes itself to its contents, so an unbounded
        // message would push the connection list off the screen. The
        // longest a failure can be is a name and a destination at their
        // maximum lengths plus a full tail.
        let (_dir, mut launcher) = launcher_on(None);
        let long = |n| "wretched-hostname".repeat(n / 17);
        launcher.error = Some(format!(
            "{} ({}@{}) ended -- exit code 255:\n{}\n(3 other sessions failed as well)",
            long(profiles::MAX_FIELD),
            long(profiles::MAX_FIELD),
            long(profiles::MAX_FIELD),
            long(600),
        ));
        let window = egui::vec2(WINDOW[0], WINDOW[1]);
        let (_, height) = status_bar_in(&mut launcher, window);
        assert!(
            height < window.y / 2.0,
            "the bar took {height} of {}",
            window.y
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_session_count_keeps_its_place_beside_a_long_failure() {
        // Why the count is laid out first: it is one short piece of text
        // competing with several lines of whatever SSH had to say, and the
        // obvious order leaves it pushed off the right-hand edge.
        let (_dir, mut launcher) = launcher_on(None);
        // /bin/sleep stands in for the session binary, and the destination
        // -- always the first argument -- is how long it sleeps for. Long
        // enough that it is certainly still running when the bar is laid
        // out, since an exited one would leave no count to look at.
        launcher
            .sessions
            .start_with(&PathBuf::from("/bin/sleep"), &named("work", "5"))
            .unwrap();
        launcher.error = Some(
            "work (alice@10.0.0.5) ended -- exit code 255:\n\
             alice@10.0.0.5: Permission denied (publickey,keyboard-interactive)."
                .into(),
        );

        let painted = status_bar(&mut launcher);
        let count = painted.text("1 session");
        let message = painted.text("Permission denied");
        assert!(count.right() <= BAR_WIDTH, "pushed off the edge: {count:?}");
        assert!(
            message.right() <= count.left() + 1.0,
            "{message:?} overlaps {count:?}"
        );
    }
}
