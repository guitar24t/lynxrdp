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

use std::path::PathBuf;

use anyhow::{Context, Result};
use eframe::egui;

use crate::launch::Sessions;
use crate::profiles::{self, Profile, Profiles};

/// Open the launcher, returning when the window is closed.
pub fn run(path: PathBuf) -> Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("LynxRDP")
        // Matches StartupWMClass in the .desktop entry, which is how a Linux
        // desktop attaches a running window to its launcher icon.
        .with_app_id(crate::APP_ID)
        .with_inner_size([780.0, 500.0])
        .with_min_inner_size([560.0, 360.0]);
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
        Box::new(move |_cc| Ok(Box::new(Launcher::new(path)))),
    )
    .map_err(|e| anyhow::anyhow!("could not open the launcher window: {e}"))
}

/// Height of one row in the connection list.
const ROW_HEIGHT: f32 = 44.0;

/// Space between the edge of a row and its text.
const ROW_PADDING: f32 = 10.0;

/// Colour of the message line when it is carrying a problem.
const ERROR_COLOUR: egui::Color32 = egui::Color32::from_rgb(0xC0, 0x39, 0x2B);

/// Which screen is showing.
enum View {
    List,
    // Boxed: the editor is much larger than the other variant, and every
    // View would otherwise be that size.
    Edit(Box<Editor>),
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

/// The application state.
struct Launcher {
    path: PathBuf,
    store: Profiles,
    view: View,
    selected: Option<usize>,
    sessions: Sessions,
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
        Self {
            path,
            store,
            view: View::List,
            selected,
            sessions: Sessions::default(),
            status: String::new(),
            error,
            confirming_delete: None,
            load_failed,
        }
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

    // ---- views ---------------------------------------------------------

    fn list_ui(&mut self, ui: &mut egui::Ui) {
        let mut connect_to: Option<usize> = None;
        let mut edit: Option<usize> = None;
        let mut move_aside = false;

        ui.horizontal(|ui| {
            ui.heading("Saved connections");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Disabled, not merely warned about: the first New -> Save
                // is what would replace a file full of connections with the
                // one that was just typed.
                let new = ui
                    .add_enabled(!self.load_failed, egui::Button::new("New"))
                    .on_disabled_hover_text(
                        "The connections file could not be read; adding one now would replace it.",
                    );
                if new.clicked() {
                    let name = self.store.unique_name("New connection");
                    self.view = View::Edit(Box::new(Editor::new(Profile::new(name), None)));
                    self.confirming_delete = None;
                }
            });
        });
        ui.separator();

        if self.load_failed {
            // An empty list here means "unreadable", not "none saved", and
            // the difference matters enough to say so where the list would
            // have been rather than only in the status line.
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    "The connections file could not be read, so nothing is listed \
                     and saving is off.",
                );
                if ui
                    .button("Move it aside")
                    .on_hover_text("Renames it to connections.toml.bad and starts empty.")
                    .clicked()
                {
                    move_aside = true;
                }
            });
            ui.separator();
        }

        if self.store.items.is_empty() && !self.load_failed {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label("No connections yet.");
                ui.label("Choose New to add one.");
            });
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - 40.0)
            .show(ui, |ui| {
                for index in 0..self.store.items.len() {
                    let (name, destination) = {
                        let p = &self.store.items[index];
                        (p.name.clone(), p.destination())
                    };
                    let selected = self.selected == Some(index);
                    // The name leads and the address supports it, so they are
                    // set apart by size and weight rather than being two
                    // identical lines. A LayoutJob is the only way to mix
                    // styles inside one clickable widget.
                    let visuals = ui.visuals();
                    let (title, subtitle) = if selected {
                        (
                            visuals.strong_text_color(),
                            visuals.strong_text_color().gamma_multiply(0.75),
                        )
                    } else {
                        (visuals.text_color(), visuals.weak_text_color())
                    };
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        &name,
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(15.0),
                            color: title,
                            ..Default::default()
                        },
                    );
                    job.append(
                        &format!("\n{destination}"),
                        0.0,
                        egui::TextFormat {
                            font_id: egui::FontId::proportional(12.0),
                            color: subtitle,
                            ..Default::default()
                        },
                    );
                    // Painted by hand rather than with a SelectableLabel:
                    // that widget sizes itself to its text and centres it, so
                    // only the words would be clickable and the names would
                    // wander with their length. A list reads down its left
                    // edge, and the whole row should be the target.
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), ROW_HEIGHT),
                        egui::Sense::click(),
                    );
                    if ui.is_rect_visible(rect) {
                        let widget = ui.style().interact_selectable(&response, selected);
                        if selected || response.hovered() || response.has_focus() {
                            ui.painter().rect(
                                rect,
                                widget.corner_radius,
                                widget.weak_bg_fill,
                                widget.bg_stroke,
                                egui::StrokeKind::Inside,
                            );
                        }
                        let galley = ui.painter().layout_job(job);
                        let pos = egui::pos2(
                            rect.left() + ROW_PADDING,
                            rect.center().y - galley.size().y / 2.0,
                        );
                        ui.painter().galley(pos, galley, title);
                    }
                    ui.add_space(2.0);
                    if response.clicked() {
                        self.selected = Some(index);
                        self.confirming_delete = None;
                    }
                    // Double-click is the fastest way in, and the one people
                    // will try first.
                    if response.double_clicked() {
                        self.selected = Some(index);
                        connect_to = Some(index);
                    }
                }
            });

        ui.separator();
        ui.horizontal(|ui| {
            let has_selection = self.selected.is_some();
            if ui
                .add_enabled(has_selection, egui::Button::new("Connect"))
                .clicked()
            {
                connect_to = self.selected;
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Edit"))
                .clicked()
            {
                edit = self.selected;
            }
            let confirming = self
                .selected
                .and_then(|i| self.store.items.get(i))
                .map(|p| self.confirming_delete.as_deref() == Some(p.name.as_str()))
                .unwrap_or(false);
            let label = if confirming {
                "Really delete?"
            } else {
                "Delete"
            };
            if ui
                .add_enabled(has_selection, egui::Button::new(label))
                .clicked()
            {
                self.delete_clicked(confirming);
            }
        });

        if let Some(index) = edit {
            if let Some(p) = self.store.items.get(index).cloned() {
                let original = p.name.clone();
                self.view = View::Edit(Box::new(Editor::new(p, Some(original))));
                self.confirming_delete = None;
            }
        }
        if let Some(index) = connect_to {
            self.connect(index);
        }
        if move_aside {
            self.move_bad_file_aside();
        }
    }

    /// Delete needs two clicks: the first arms it, the second does it. A
    /// modal would be heavier, and an undo would need somewhere to put the
    /// deleted entry.
    fn delete_clicked(&mut self, confirming: bool) {
        let Some(index) = self.selected else { return };
        let Some(name) = self.store.items.get(index).map(|p| p.name.clone()) else {
            return;
        };
        if confirming {
            self.store.remove(&name);
            self.confirming_delete = None;
            self.selected = if self.store.items.is_empty() {
                None
            } else {
                Some(index.min(self.store.items.len() - 1))
            };
            self.status = format!("Deleted {name}");
            self.save();
        } else {
            self.confirming_delete = Some(name);
        }
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui) {
        let View::Edit(editor) = &mut self.view else {
            return;
        };
        let adding = editor.original.is_none();
        ui.heading(if adding {
            "New connection"
        } else {
            "Edit connection"
        });
        ui.separator();

        egui::Grid::new("connection")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut editor.profile.name);
                ui.end_row();

                ui.label("Host");
                ui.text_edit_singleline(&mut editor.profile.host);
                ui.end_row();

                ui.label("User");
                ui.text_edit_singleline(&mut editor.profile.user);
                ui.end_row();

                ui.label("SSH port");
                ui.add(
                    egui::TextEdit::singleline(&mut editor.ssh_port)
                        .hint_text("default")
                        .desired_width(90.0),
                );
                ui.end_row();

                ui.label("Identity file");
                ui.text_edit_singleline(&mut editor.identity);
                ui.end_row();

                ui.label("Screen size");
                ui.add(
                    egui::TextEdit::singleline(&mut editor.size)
                        .hint_text("server default, e.g. 1920x1080")
                        .desired_width(200.0),
                );
                ui.end_row();

                ui.label("LynxRDP port");
                ui.add(
                    egui::TextEdit::singleline(&mut editor.remote_port)
                        .hint_text(lynxrdp_proto::DEFAULT_PORT.to_string())
                        .desired_width(90.0),
                );
                ui.end_row();

                ui.label("SSH options");
                ui.add(
                    egui::TextEdit::multiline(&mut editor.ssh_options)
                        .hint_text("one per line, e.g. ProxyJump=bastion")
                        .desired_rows(3),
                );
                ui.end_row();

                ui.label("");
                ui.vertical(|ui| {
                    ui.checkbox(&mut editor.profile.fullscreen, "Start fullscreen");
                    ui.checkbox(
                        &mut editor.profile.dynamic_resize,
                        "Resize the remote screen with the window",
                    );
                    ui.checkbox(&mut editor.profile.clipboard, "Share the clipboard");
                });
                ui.end_row();
            });

        ui.separator();
        let mut save = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                save = true;
            }
            if ui.button("Cancel").clicked() {
                cancel = true;
            }
        });

        if cancel {
            self.view = View::List;
            self.error = None;
            return;
        }
        if save {
            self.save_editor();
        }
    }

    fn save_editor(&mut self) {
        let View::Edit(editor) = &self.view else {
            return;
        };
        match editor.collect() {
            Err(problem) => self.error = Some(problem),
            Ok(profile) => {
                let original = editor.original.clone();
                // Checked before anything is written, and before the status
                // line claims success. Entries are keyed by name, so saving
                // onto a name already in use would not add a connection --
                // upsert would replace the other one, and the host that was
                // there would be gone with no way back.
                if self.store.name_taken(&profile.name, original.as_deref()) {
                    self.error = Some(format!(
                        "There is already a connection called {:?}. Names have to be unique, \
                         so choose another.",
                        profile.name
                    ));
                    return;
                }
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
        // The count is placed first, from the right, so a long message
        // cannot push it off the edge: a failed connection brings several
        // lines of whatever SSH had to say.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
            let running = self.sessions.count();
            if running > 0 {
                ui.label(format!("{running} session(s) open"));
            }
            let message = match &self.error {
                Some(error) => egui::RichText::new(error.as_str()).color(ERROR_COLOUR),
                None => egui::RichText::new(self.status.as_str()),
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
}

impl eframe::App for Launcher {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sessions.reap();
        self.drain_failures();

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| self.status_ui(ui));

        egui::CentralPanel::default().show(ctx, |ui| match self.view {
            View::List => self.list_ui(ui),
            View::Edit(_) => self.editor_ui(ui),
        });

        // A session may exit at any time and the count in the status bar
        // should notice without needing a mouse move.
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
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

    // ---- the status bar ------------------------------------------------
    //
    // egui needs no window, only eframe does, so the one part of the
    // launcher that is pure layout can be run and measured here.

    /// Width used for the laid-out status bar. Much narrower than the
    /// launcher's real window, so that a realistic SSH complaint has to wrap
    /// rather than happening to fit.
    const BAR_WIDTH: f32 = 300.0;

    /// Lay out the status bar and return every piece of text that was drawn
    /// with the rectangle it occupies, in screen coordinates.
    fn status_bar(launcher: &mut Launcher) -> Vec<(String, egui::Rect)> {
        status_bar_in(launcher, egui::vec2(BAR_WIDTH, 300.0)).0
    }

    /// The same, in a window of `window` points, also returning how tall the
    /// bar itself ended up.
    fn status_bar_in(
        launcher: &mut Launcher,
        window: egui::Vec2,
    ) -> (Vec<(String, egui::Rect)>, f32) {
        let ctx = egui::Context::default();
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
        let mut found = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut found);
        }
        (found, height)
    }

    fn collect_text(shape: &egui::Shape, out: &mut Vec<(String, egui::Rect)>) {
        match shape {
            // The galley's own rect already accounts for its alignment,
            // which a rect built from the position and the size would not:
            // a right-aligned galley is laid out to the left of its
            // position, not to the right.
            egui::Shape::Text(text) => out.push((
                text.galley.text().to_string(),
                text.galley.rect.translate(text.pos.to_vec2()),
            )),
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, out);
                }
            }
            _ => {}
        }
    }

    /// The one drawn text containing `needle`.
    fn drawn(texts: &[(String, egui::Rect)], needle: &str) -> egui::Rect {
        let mut hits = texts.iter().filter(|(text, _)| text.contains(needle));
        let (_, rect) = hits.next().unwrap_or_else(|| {
            let all: Vec<&String> = texts.iter().map(|(t, _)| t).collect();
            panic!("{needle:?} was not drawn; the bar had {all:?}")
        });
        assert!(hits.next().is_none(), "{needle:?} was drawn more than once");
        *rect
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
        let rect = drawn(&status_bar(&mut launcher), "Saved work");
        assert!(rect.left() < BAR_WIDTH / 4.0, "{rect:?}");
    }

    #[test]
    fn a_long_failure_wraps_instead_of_running_off_the_edge() {
        let (_dir, mut launcher) = launcher_on(None);
        launcher.status = "Saved work".into();
        let one_line = drawn(&status_bar(&mut launcher), "Saved work").height();

        launcher.status.clear();
        launcher.error = Some(
            "work (alice@10.0.0.5) ended -- exit code 255:\n\
             alice@10.0.0.5: Permission denied (publickey,keyboard-interactive)."
                .into(),
        );
        let rect = drawn(&status_bar(&mut launcher), "Permission denied");
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
        // The launcher's own starting size, from `run` above.
        let window = egui::vec2(780.0, 500.0);
        let (_, height) = status_bar_in(&mut launcher, window);
        assert!(height < window.y / 2.0, "the bar took {height} of 500");
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

        let texts = status_bar(&mut launcher);
        let count = drawn(&texts, "session(s) open");
        let message = drawn(&texts, "Permission denied");
        assert!(count.right() <= BAR_WIDTH, "pushed off the edge: {count:?}");
        assert!(
            message.right() <= count.left() + 1.0,
            "{message:?} overlaps {count:?}"
        );
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
}
