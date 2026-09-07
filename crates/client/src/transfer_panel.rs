//! Non-modal graphical transfer controls. File drops never open this window.
use crate::gui_paint::Renderer;
use eframe::egui::{self, Align2, RichText};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use winit::{event::WindowEvent, window::Window};

const PROGRESS_DELAY: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct Panel {
    pub open: bool,
    pub remote: String,
    pub local: String,
    pub replace: bool,
    pub message: String,
    seen_message: String,
    toast_until: Option<Instant>,
    dismissed: bool,
    had_active: bool,
    active_since: Option<Instant>,
    history: std::collections::VecDeque<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Reconnect,
    Download,
    CancelAll,
    Cancel(u64),
}
#[derive(Clone, Debug)]
pub struct Transfer {
    pub id: u64,
    pub name: String,
    pub progress: Option<(u64, u64)>,
}
impl Panel {
    /// Keep routine activity in the manually opened details, without a toast.
    pub fn record(&mut self, message: String) {
        self.history.push_front(message);
        self.history.truncate(20);
    }

    pub fn server_notice(&mut self, message: String) {
        // Existing servers send readiness as an untyped Notice. Keep this
        // compatibility check narrow so partial failures still reach the user.
        if message
            .strip_suffix(" copied file(s) ready. Paste into a folder in the remote session.")
            .is_some_and(|count| count.parse::<usize>().is_ok())
        {
            self.record(message);
        } else {
            self.notify(message);
        }
    }

    pub fn notify(&mut self, message: String) {
        self.message = message.clone();
        self.record(message);
        self.dismissed = false;
        self.toast_until = Some(Instant::now() + Duration::from_secs(4));
    }

    pub fn destination(&self) -> Result<PathBuf, String> {
        if self.remote.is_empty() || self.local.is_empty() {
            return Err("Enter both the remote file and local destination.".into());
        }
        Ok(PathBuf::from(&self.local))
    }
    pub fn visible(&self) -> bool {
        self.open || !self.dismissed && self.toast_until.is_some_and(|until| Instant::now() < until)
    }
    pub fn show(&mut self, ctx: &egui::Context, queued: usize, active: &[Transfer]) -> Vec<Action> {
        let mut actions = Vec::new();
        let active_now = !active.is_empty() || queued != 0;
        let now = Instant::now();
        let progress_ready = if active_now {
            let since = *self.active_since.get_or_insert(now);
            let remaining = PROGRESS_DELAY.saturating_sub(now.saturating_duration_since(since));
            if !remaining.is_zero() {
                ctx.request_repaint_after(remaining);
            }
            remaining.is_zero()
        } else {
            self.active_since = None;
            false
        };
        let changed_message = self.message != self.seen_message;
        if self.had_active && !active_now && !changed_message {
            // Clear a preparation message as soon as its transfer finishes.
            self.toast_until = None;
        }
        self.had_active = active_now;
        if changed_message {
            self.seen_message = self.message.clone();
            self.toast_until = Some(Instant::now() + Duration::from_secs(4));
            self.dismissed = false;
        }
        if let Some(until) = self.toast_until.filter(|_| !self.dismissed) {
            if until > Instant::now() {
                ctx.request_repaint_after(until.saturating_duration_since(Instant::now()));
            }
        }
        let available = ctx.screen_rect().size();
        let width = 410.0f32.min((available.x - 40.0).max(160.0));
        let mut open = self.open;
        if open {
            egui::Window::new("File transfers")
                .id(egui::Id::new("file_transfers"))
                .open(&mut open)
                .default_pos(egui::pos2((available.x - width - 28.0).max(8.0), 64.0))
                .default_height(300.0)
                .default_width(width)
                .min_width(160.0)
                .max_width(width)
                .max_height((available.y - 100.0).max(80.0))
                .resizable(false)
                .collapsible(false)
                .vscroll(true)
                .show(ctx, |ui| {
                    ui.label(RichText::new("Drop files to send them automatically").strong());
                    ui.label("Your desktop stays available while files copy.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.heading("Activity");
                        if !active.is_empty() || queued != 0 {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Cancel all").clicked() {
                                        actions.push(Action::CancelAll);
                                    }
                                },
                            );
                        }
                    });
                    if active.is_empty() && queued == 0 {
                        ui.weak("No transfers in progress");
                    }
                    if queued != 0 {
                        ui.label(format!("{queued} files waiting to send"));
                    }
                    for item in active {
                        ui.push_id(item.id, |ui| {
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.set_min_width((width - 52.0).max(100.0));
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::Label::new(RichText::new(&item.name).strong())
                                            .truncate(),
                                    )
                                    .on_hover_text(&item.name);
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button("Cancel").clicked() {
                                                actions.push(Action::Cancel(item.id));
                                            }
                                        },
                                    );
                                });
                                if let Some((done, total)) = item.progress {
                                    let fraction = if total == 0 {
                                        0.0
                                    } else {
                                        done as f32 / total as f32
                                    };
                                    ui.add(
                                        egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                                            .text(format!("{} of {}", bytes(done), bytes(total))),
                                    );
                                } else {
                                    ui.weak("Preparing transfer...");
                                }
                            });
                        });
                    }
                    if !self.message.is_empty() {
                        ui.add_space(8.0);
                        ui.label(&self.message);
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    egui::CollapsingHeader::new("Download a remote file").show(ui, |ui| {
                        ui.label("Remote file");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.remote)
                                .id(egui::Id::new("download_remote_path"))
                                .hint_text("/home/user/Documents/report.pdf")
                                .desired_width(f32::INFINITY),
                        );
                        ui.label("Save to this computer");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.local)
                                .id(egui::Id::new("download_local_path"))
                                .hint_text("Full local destination path")
                                .desired_width(f32::INFINITY),
                        );
                        ui.checkbox(&mut self.replace, "Replace an existing destination file");
                        if ui
                            .add_enabled(
                                !self.remote.is_empty() && !self.local.is_empty(),
                                egui::Button::new("Download"),
                            )
                            .clicked()
                        {
                            actions.push(Action::Download);
                        }
                    });
                    if !self.history.is_empty() {
                        egui::CollapsingHeader::new("Recent activity").show(ui, |ui| {
                            for message in &self.history {
                                ui.label(message);
                                ui.separator();
                            }
                        });
                    }
                    ui.add_space(4.0);
                    ui.weak("Dropped files never replace existing files.");
                });
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                open = false;
            }
            self.open = open;
        } else if !self.dismissed && (progress_ready || self.visible()) {
            egui::Area::new(egui::Id::new("transfer_notification"))
                .anchor(Align2::RIGHT_BOTTOM, [-16.0, -16.0])
                .order(egui::Order::Foreground)
                .interactable(false)
                .show(ctx, |ui| {
                    egui::Frame::group(ui.style())
                        .fill(ui.visuals().panel_fill)
                        .corner_radius(10.0)
                        .inner_margin(12.0)
                        .show(ui, |ui| {
                            ui.set_width(width.min(300.0));
                            if progress_ready {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(format!("Copying {} file(s)", active.len() + queued));
                                });
                                if let Some(item) = active.first() {
                                    ui.add(egui::Label::new(&item.name).truncate());
                                }
                                let (done, total) = active.iter().filter_map(|t| t.progress).fold(
                                    (0u64, 0u64),
                                    |(d, t), (next_d, next_t)| {
                                        (d.saturating_add(next_d), t.saturating_add(next_t))
                                    },
                                );
                                let preparing = active.iter().any(|t| t.progress.is_none());
                                ui.add(
                                    egui::ProgressBar::new(if total == 0 {
                                        0.0
                                    } else {
                                        (done as f32 / total as f32).clamp(0.0, 1.0)
                                    })
                                    .animate(preparing)
                                    .text(if preparing {
                                        "Preparing files...".into()
                                    } else {
                                        format!("{} of {}", bytes(done), bytes(total))
                                    }),
                                );
                                ctx.request_repaint_after(Duration::from_millis(100));
                            } else if !self.message.is_empty() {
                                ui.add(egui::Label::new(&self.message).wrap());
                            }
                        });
                });
        }
        actions
    }
}
fn bytes(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else if n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

pub struct Gui {
    pub ctx: egui::Context,
    state: egui_winit::State,
    renderer: Renderer,
    repaint_at: Option<Instant>,
}
pub(crate) struct Prepared {
    output: egui::FullOutput,
    actions: Vec<Action>,
    pub bounds: Option<lynxrdp_proto::Rect>,
}
impl Gui {
    pub fn new(window: &Window) -> Self {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        ctx.set_theme(egui::Theme::Dark);
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            window.theme(),
            None,
        );
        Self {
            ctx,
            state,
            renderer: Renderer::default(),
            repaint_at: None,
        }
    }
    pub fn event(&mut self, window: &Window, event: &WindowEvent) -> egui_winit::EventResponse {
        self.state.on_window_event(window, event)
    }
    pub(crate) fn prepare(
        &mut self,
        window: &Window,
        panel: &mut Panel,
        queued: usize,
        active: &[Transfer],
        notice: Option<&crate::app::Notice>,
    ) -> Prepared {
        let mut actions = Vec::new();
        let output = self.ctx.run(self.state.take_egui_input(window), |ctx| {
            actions = panel.show(ctx, queued, active);
            if let Some(notice) = notice {
                egui::Window::new(&notice.headline)
                    .id(egui::Id::new("connection_notice"))
                    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                    .collapsible(false)
                    .resizable(false)
                    .default_width(420.0)
                    .max_width((ctx.screen_rect().width() - 48.0).max(160.0))
                    .show(ctx, |ui| {
                        ui.colored_label(crate::gui_paint::color(notice.colour), &notice.detail);
                        ui.label(notice.hint);
                        if notice.headline != "Session ended"
                            && ui.button("Reconnect now").clicked()
                        {
                            actions.push(Action::Reconnect);
                        }
                    });
            }
        });
        self.state
            .handle_platform_output(window, output.platform_output.clone());
        let delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |v| v.repaint_delay);
        self.repaint_at = Instant::now().checked_add(delay.max(Duration::from_millis(16)));
        let size = window.inner_size();
        let screen = egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(size.width as f32, size.height as f32),
        );
        let bounds = output
            .shapes
            .iter()
            .fold(egui::Rect::NOTHING, |bounds, shape| {
                bounds.union(
                    shape
                        .shape
                        .visual_bounding_rect()
                        .intersect(shape.clip_rect),
                )
            });
        let bounds = (bounds * output.pixels_per_point)
            .expand(1.0)
            .intersect(screen);
        let bounds = if bounds.is_positive() {
            let (x, y) = (bounds.min.x.floor() as u32, bounds.min.y.floor() as u32);
            Some(lynxrdp_proto::Rect::new(
                x,
                y,
                bounds.max.x.ceil() as u32 - x,
                bounds.max.y.ceil() as u32 - y,
            ))
        } else {
            None
        };
        Prepared {
            output,
            actions,
            bounds,
        }
    }
    pub fn repaint_in(&self) -> Duration {
        self.repaint_at.map_or(Duration::MAX, |time| {
            time.saturating_duration_since(Instant::now())
        })
    }
    pub(crate) fn paint(
        &mut self,
        frame: Prepared,
        dst: &mut [u32],
        width: u32,
        height: u32,
    ) -> Vec<Action> {
        self.renderer
            .paint(&self.ctx, frame.output, dst, width, height);
        frame.actions
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_do_not_open_the_details_or_authorize_overwrites() {
        let ctx = egui::Context::default();
        let mut panel = Panel {
            message: "Copying a file".into(),
            ..Default::default()
        };
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            panel.show(ctx, 1, &[]);
        });
        assert!(!panel.open);
        assert!(!panel.replace);
        assert!(panel.visible());
        assert!(panel.destination().is_err());
    }
    #[test]
    fn automatic_progress_and_errors_disappear_without_opening_details() {
        let ctx = egui::Context::default();
        let mut panel = Panel::default();
        panel.notify("Preparing copied files".into());
        let active = [Transfer {
            id: 1,
            name: "report.pdf".into(),
            progress: Some((10, 100)),
        }];
        frame(&ctx, &mut panel, vec![], &active);
        panel.active_since = Some(Instant::now() - PROGRESS_DELAY);
        let (output, _) = frame(&ctx, &mut panel, vec![], &active);
        text_position(&output, "report.pdf");
        assert!(!panel.open);
        frame(&ctx, &mut panel, vec![], &[]);
        let (output, _) = frame(&ctx, &mut panel, vec![], &[]);
        assert!(output.shapes.is_empty());
        panel.notify("Transfer failed: disk full".into());
        frame(&ctx, &mut panel, vec![], &[]);
        let (output, _) = frame(&ctx, &mut panel, vec![], &[]);
        text_position(&output, "Transfer failed: disk full");
        assert!(!panel.open);
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay <= Duration::from_secs(4)
        );
        panel.toast_until = Some(Instant::now() - Duration::from_secs(1));
        frame(&ctx, &mut panel, vec![], &[]);
        let (output, _) = frame(&ctx, &mut panel, vec![], &[]);
        assert!(output.shapes.is_empty());
    }
    #[test]
    fn clipboard_success_is_silent_but_partial_failures_are_visible() {
        let ctx = egui::Context::default();
        let mut panel = Panel::default();
        panel.record("Files copied".into());
        panel.server_notice(
            "2 copied file(s) ready. Paste into a folder in the remote session.".into(),
        );
        frame(&ctx, &mut panel, vec![], &[]);
        let (output, _) = frame(&ctx, &mut panel, vec![], &[]);
        assert!(output.shapes.is_empty());
        assert!(!panel.visible());
        assert_eq!(panel.history.len(), 2);
        panel.server_notice(
            "Only 1 of 2 copied files are ready to paste. Copy the missing files again to retry."
                .into(),
        );
        frame(&ctx, &mut panel, vec![], &[]);
        assert!(panel.visible());
        assert!(!panel.open);
    }
    #[test]
    fn quick_transfers_never_flash_and_each_new_batch_waits() {
        let ctx = egui::Context::default();
        let mut panel = Panel::default();
        let active = [Transfer {
            id: 1,
            name: "copied.txt".into(),
            progress: Some((0, 10)),
        }];
        let (output, _) = frame(&ctx, &mut panel, vec![], &active);
        assert!(output.shapes.is_empty());
        frame(&ctx, &mut panel, vec![], &[]);
        let (output, _) = frame(&ctx, &mut panel, vec![], &[]);
        assert!(output.shapes.is_empty());
        assert!(panel.active_since.is_none());
        frame(&ctx, &mut panel, vec![], &active);
        panel.active_since = Some(Instant::now() - PROGRESS_DELAY);
        frame(&ctx, &mut panel, vec![], &active);
        let (output, _) = frame(&ctx, &mut panel, vec![], &active);
        text_position(&output, "copied.txt");
        frame(&ctx, &mut panel, vec![], &[]);
        frame(&ctx, &mut panel, vec![], &[]);
        let (output, _) = frame(&ctx, &mut panel, vec![], &active);
        assert!(output.shapes.is_empty());
        panel.notify("Transfer failed".into());
        frame(&ctx, &mut panel, vec![], &active);
        let (output, _) = frame(&ctx, &mut panel, vec![], &active);
        text_position(&output, "Transfer failed");
    }
    fn frame(
        ctx: &egui::Context,
        panel: &mut Panel,
        events: Vec<egui::Event>,
        active: &[Transfer],
    ) -> (egui::FullOutput, Vec<Action>) {
        let mut actions = Vec::new();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 700.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                actions = panel.show(ctx, 0, active);
            },
        );
        (output, actions)
    }
    fn text_position(output: &egui::FullOutput, label: &str) -> egui::Pos2 {
        output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    if text.galley.job.text == label {
                        return Some(text.pos + text.galley.size() * 0.5);
                    }
                }
                None
            })
            .unwrap_or_else(|| panic!("missing graphical label: {label}"))
    }
    fn click(
        ctx: &egui::Context,
        panel: &mut Panel,
        pos: egui::Pos2,
        active: &[Transfer],
    ) -> (egui::FullOutput, Vec<Action>) {
        frame(
            ctx,
            panel,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            active,
        );
        frame(
            ctx,
            panel,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
            active,
        )
    }
    #[test]
    fn graphical_cancel_uses_the_transfer_id_and_fields_support_selection_and_paste() {
        let ctx = egui::Context::default();
        let mut panel = Panel {
            open: true,
            ..Default::default()
        };
        let active = [Transfer {
            id: 77,
            name: "report.pdf".into(),
            progress: Some((5, 10)),
        }];
        frame(&ctx, &mut panel, vec![], &active);
        let (output, _) = frame(&ctx, &mut panel, vec![], &active);
        let cancel = text_position(&output, "Cancel");
        assert_eq!(
            click(&ctx, &mut panel, cancel, &active).1,
            vec![Action::Cancel(77)]
        );
        let (output, _) = frame(&ctx, &mut panel, vec![], &active);
        let expand = text_position(&output, "Download a remote file");
        click(&ctx, &mut panel, expand, &active);
        // Finish the collapsing-header animation before interacting with its contents.
        ctx.style_mut(|style| style.animation_time = 0.0);
        frame(&ctx, &mut panel, vec![], &active);
        let id = egui::Id::new("download_remote_path");
        let pos = ctx
            .read_response(id)
            .expect("a real text field")
            .rect
            .center();
        click(&ctx, &mut panel, pos, &active);
        frame(
            &ctx,
            &mut panel,
            vec![egui::Event::Text("/home/alex/old.txt".into())],
            &active,
        );
        assert_eq!(panel.remote, "/home/alex/old.txt");
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        frame(
            &ctx,
            &mut panel,
            vec![
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Paste("/home/alex/new.txt".into()),
            ],
            &active,
        );
        assert_eq!(panel.remote, "/home/alex/new.txt");
        assert!(!panel.replace);
    }
}
