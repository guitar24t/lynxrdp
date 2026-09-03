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
use crate::profiles::{Profile, Profiles};

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
}

impl Launcher {
    fn new(path: PathBuf) -> Self {
        let (store, error) = match Profiles::load(&path) {
            Ok(store) => (store, None),
            // A broken file must not leave a blank window with no
            // explanation, and must not be silently overwritten either.
            Err(e) => (
                Profiles::default(),
                Some(format!(
                    "Could not read {}: {e:#}. Fix or move the file; saving now would overwrite it.",
                    path.display()
                )),
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
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.store.save(&self.path) {
            self.error = Some(format!("Could not save connections: {e:#}"));
        }
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

        ui.horizontal(|ui| {
            ui.heading("Saved connections");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("New").clicked() {
                    let name = self.store.unique_name("New connection");
                    self.view = View::Edit(Box::new(Editor::new(Profile::new(name), None)));
                    self.confirming_delete = None;
                }
            });
        });
        ui.separator();

        if self.store.items.is_empty() {
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
}

impl eframe::App for Launcher {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.sessions.reap();

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::from_rgb(0xC0, 0x39, 0x2B), error);
                } else {
                    ui.label(&self.status);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let running = self.sessions.count();
                    if running > 0 {
                        ui.label(format!("{running} session(s) open"));
                    }
                });
            });
        });

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
