//! Answering ssh's prompts in a window.
//!
//! A session started from the connection manager has nowhere to be asked
//! anything. `launch.rs` gives it a null stdin, a desktop gives it no
//! controlling terminal, and macOS has no `DISPLAY` for the traditional
//! X11 askpass to key off. So ssh refuses -- "Host key verification failed",
//! "Permission denied" -- and the entry point the README points people at
//! works only for hosts already in `known_hosts` reachable with a
//! non-interactive key. The command line, meanwhile, prompts perfectly well.
//!
//! OpenSSH already has the hook for this: `SSH_ASKPASS` names a program it
//! runs as `<program> "<prompt>"` and reads the answer from, and
//! `SSH_ASKPASS_REQUIRE=force` (8.4 and later) makes it prefer that program
//! over `/dev/tty`. This module is that program. It is this same binary --
//! `main` checks for it before it parses arguments, because ssh passes the
//! prompt as a bare positional argument -- run as a separate process, which
//! it has to be: eframe holds the launcher's winit event loop and a process
//! gets only one.
//!
//! Two things it deliberately does not do.
//!
//! It stores nothing. No cache, no keychain, no "remember this". A passphrase
//! lives in this process for as long as the window is open and goes to ssh
//! down a pipe; the client's no-secrets-on-disk rule has no exception for
//! convenience, and an askpass that quietly kept a copy would be the largest
//! one imaginable.
//!
//! And it is wired up *only* for sessions the connection manager started,
//! through an environment marker the launcher sets on itself and its children
//! inherit. A session typed at a terminal keeps prompting at that terminal,
//! where the user is already looking.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use eframe::egui;

use crate::theme;

/// Set on the connection manager's own process, and inherited by the sessions
/// it starts. Its presence is the whole of "this session came from the GUI".
pub const GUI_MARKER: &str = "LYNXRDP_GUI_ASKPASS";

/// Set on the `ssh` process, and so present in the environment ssh hands the
/// askpass program. Its presence is the whole of "this invocation is the
/// helper", which is why it is checked before the argument parser sees a
/// prompt that looks nothing like a command line.
pub const HELPER_MARKER: &str = "LYNXRDP_ASKPASS";

/// What the prompt is asking for, which decides what the window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A passphrase, a password, a token code: something to type, unechoed.
    Secret,
    /// A yes-or-no question, which is very nearly always the host key one.
    Confirm,
}

/// What `ssh` wants back for a [`Kind::Confirm`]. It matches on the word, not
/// on a leading letter: "y" is not an answer OpenSSH accepts.
const YES: &str = "yes";
const NO: &str = "no";

/// Which kind of question `prompt` is.
///
/// Text is all there is to go on -- `SSH_ASKPASS` passes a string and nothing
/// else, no flag for "this one echoes" -- and getting it wrong is worse than
/// having no askpass at all: a first connection to a new host asks
///
/// ```text
/// The authenticity of host 'gpu-01 (10.0.0.5)' can't be established.
/// ED25519 key fingerprint is SHA256:HrxE0...
/// Are you sure you want to continue connecting (yes/no/[fingerprint])?
/// ```
///
/// and answering it with a password box gives the user a masked field for a
/// question whose only valid answers are "yes" and "no". That is the single
/// most common first-connection case, so it is the one this must get right.
pub fn classify(prompt: &str) -> Kind {
    let lower = prompt.to_ascii_lowercase();
    // "(yes/no)", "(yes/no/[fingerprint])" and the "Please type 'yes', 'no'
    // or the fingerprint" that follows a mistyped answer.
    if lower.contains("yes/no") || lower.contains("'yes'") || lower.contains("\"yes\"") {
        return Kind::Confirm;
    }
    Kind::Secret
}

/// Run as ssh's askpass helper, if that is what this invocation is.
///
/// Returns the exit status to leave with, or `None` when this is an ordinary
/// run of the client and `main` should carry on. Called before anything
/// touches the standard handles: the answer goes to ssh down our stdout, and
/// `console::attach_to_parent` would point that at whatever terminal the
/// launcher was started from instead.
pub fn run_if_helper() -> Option<i32> {
    let prompt = helper_prompt(std::env::var_os(HELPER_MARKER), std::env::args_os())?;
    let answer = ask(&prompt);
    match answer {
        Some(mut answer) => {
            // Written by hand rather than with println!, which panics on a
            // broken pipe -- and ssh giving up while the window is open is
            // exactly how this ends when a connection is cancelled.
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(answer.as_bytes());
            let _ = out.write_all(b"\n");
            let _ = out.flush();
            // The copy `Ask::drop` cannot reach: the dialog handed this String
            // over on the way out, so zeroing only the widget's own buffer
            // would leave the answer itself sitting in the heap until the
            // process image is written to a core file. Same best effort, same
            // limits -- see that comment.
            //
            // SAFETY: NUL is valid UTF-8, so the String stays well formed.
            unsafe { answer.as_mut_vec().fill(0) };
            Some(0)
        }
        // A non-zero exit is how an askpass says "no answer"; ssh then fails
        // the authentication rather than waiting.
        None => Some(1),
    }
}

/// The prompt this invocation was asked to put to the user, if any.
///
/// `marker` is the value of [`HELPER_MARKER`], passed in rather than read here
/// so the recognition rule can be tested without a window *and* without a test
/// setting a process-wide environment variable while its neighbours run.
///
/// The rule is deliberately narrow: the marker, exactly one argument, and that
/// argument not looking like an option or blank. That still cannot tell a
/// prompt from a destination -- `lynxrdp gpu-01` has the same shape as
/// `lynxrdp "Password: "` -- so it rests on the marker being set by us on the
/// `ssh` child alone and never exported anywhere a person would type a
/// command. The shape check is what catches the marker leaking into a shell:
/// `lynxrdp` with no arguments, with options, or with a subcommand still opens
/// the client.
fn helper_prompt<I: IntoIterator<Item = OsString>>(
    marker: Option<OsString>,
    args: I,
) -> Option<String> {
    marker?;
    let mut args = args.into_iter().skip(1);
    let prompt = args.next()?.into_string().ok()?;
    if args.next().is_some() || prompt.starts_with('-') || prompt.trim().is_empty() {
        return None;
    }
    Some(prompt)
}

/// Mark this process as the connection manager.
///
/// The sessions it starts inherit the environment, so this one call is what
/// distinguishes "started by a click" from "typed at a prompt" three
/// processes later, without `launch.rs` having to know anything about ssh.
///
/// Called from `main` before the window opens, which is also the only safe
/// place for it: setting an environment variable races with any other thread
/// reading one, and there are no other threads yet.
pub fn mark_launcher() {
    std::env::set_var(GUI_MARKER, "1");
}

/// Whether this session should route ssh's prompts through the GUI helper.
pub fn wanted() -> bool {
    std::env::var_os(GUI_MARKER).is_some()
}

/// The environment to add to `ssh` so it asks us.
///
/// Empty when this is not a GUI session, when we cannot find our own
/// executable, or when the user has already chosen an askpass -- a desktop
/// that ships one, or a line in the user's profile, is a deliberate choice
/// and outranks ours.
pub fn ssh_env() -> Vec<(String, String)> {
    if !wanted() {
        return Vec::new();
    }
    let Ok(exe) = std::env::current_exe() else {
        log::warn!("cannot find this executable, so ssh's prompts have nowhere to go");
        return Vec::new();
    };
    ssh_env_from(
        &exe,
        std::env::var_os("SSH_ASKPASS").as_deref(),
        std::env::var_os("SSH_ASKPASS_REQUIRE").as_deref(),
    )
}

/// The pure half of [`ssh_env`].
fn ssh_env_from(
    exe: &Path,
    askpass: Option<&OsStr>,
    require: Option<&OsStr>,
) -> Vec<(String, String)> {
    // `is_none_or` would say this, and is newer than the crate's MSRV.
    let unset = |v: Option<&OsStr>| !matches!(v, Some(v) if !v.is_empty());
    let mut env = Vec::new();
    if unset(askpass) {
        let Some(exe) = exe.to_str() else {
            // ssh execs the value as a path; a lossy conversion would name a
            // different file, or none.
            return Vec::new();
        };
        env.push(("SSH_ASKPASS".to_string(), exe.to_string()));
        // The marker travels with it so the helper, three processes away,
        // knows what it is. Setting it here rather than on ourselves keeps it
        // out of everything else this process might start.
        env.push((HELPER_MARKER.to_string(), "1".to_string()));
    }
    // "force" is what makes this work at all on macOS: without it OpenSSH
    // uses an askpass only when there is no tty *and* DISPLAY is set, and a
    // Mac has no DISPLAY. It also covers the launcher started from a
    // terminal, where the session inherits a controlling terminal it has no
    // window for and ssh would prompt on /dev/tty, invisibly.
    if unset(require) {
        env.push(("SSH_ASKPASS_REQUIRE".to_string(), "force".to_string()));
    }
    env
}

// ---- the window ------------------------------------------------------

/// Width of the dialog. Wide enough for a `SHA256:` fingerprint in the
/// monospace face without wrapping, since comparing one against a phone
/// screen is the entire point of showing it.
const WIDTH: f32 = 560.0;

/// Put `prompt` to the user; `None` if they cancelled.
fn ask(prompt: &str) -> Option<String> {
    let kind = classify(prompt);
    let answer = std::sync::Arc::new(std::sync::Mutex::new(None));

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("LynxRDP")
        .with_app_id(crate::APP_ID)
        .with_inner_size([WIDTH, height_for(prompt, kind)])
        .with_resizable(true)
        // A credential prompt that opens behind the window that caused it is
        // a hang as far as the user is concerned: nothing happens, and the
        // reason is hidden. The session it belongs to is a grandchild of the
        // launcher, so nothing else will raise it.
        .with_always_on_top()
        .with_active(true);
    if let Some(icon) = crate::icon::load() {
        viewport = viewport.with_icon(egui::IconData {
            rgba: icon.rgba,
            width: icon.width,
            height: icon.height,
        });
    }

    let app = Ask {
        prompt: prompt.to_string(),
        kind,
        secret: String::new(),
        focused: false,
        answer: answer.clone(),
    };
    let result = eframe::run_native(
        "LynxRDP",
        eframe::NativeOptions {
            viewport,
            // Nothing about this window survives it, and that is not left to
            // a Cargo feature to decide. eframe writes an `app.ron` only when
            // its "persistence" feature is on, which it is not today -- but
            // the thing it would write is `egui::Memory`, and egui keeps a
            // `TextEditState` per field there, undo history included. That
            // history is the passphrase. Someone turning the feature on later
            // to remember the launcher's window size would be writing
            // credentials to disk as a side effect, in a file nobody would
            // think to look in, and this module's whole promise would be gone
            // with no line of it edited. The geometry goes too: this dialog
            // and the launcher share an app id, so a persisted size here
            // would be restored as the launcher's.
            persist_window: false,
            ..Default::default()
        },
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    );
    if let Err(e) = result {
        // The session cannot recover from this, but it can say why instead of
        // dying with "Permission denied" and no explanation. stderr is kept
        // by the launcher and shown in the connection list.
        eprintln!("lynxrdp: could not open a window to ask for credentials: {e}");
        return None;
    }
    // Named rather than chained: as a tail expression the guard would still
    // be alive when `answer` itself is dropped at the end of the block.
    let mut slot = answer.lock().ok()?;
    slot.take()
}

/// How tall to open the window, from how much prompt there is to show.
///
/// A host-key question is five lines and a passphrase prompt is one; opening
/// both at the same height means one is mostly empty and the other scrolls
/// for no reason.
fn height_for(prompt: &str, kind: Kind) -> f32 {
    let lines = prompt.lines().count().clamp(1, 8) as f32;
    let chrome = match kind {
        Kind::Confirm => 96.0,
        Kind::Secret => 128.0,
    };
    chrome + lines * 20.0
}

struct Ask {
    prompt: String,
    kind: Kind,
    secret: String,
    /// Whether the text field has been given focus yet. Once, on the first
    /// frame: asking every frame would fight anyone who clicked elsewhere.
    focused: bool,
    answer: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl Drop for Ask {
    fn drop(&mut self) {
        // Best effort, and worth the two lines anyway: it keeps the last copy
        // out of a core dump. It cannot be more than that -- a String that
        // grew a character at a time left its shorter selves in freed blocks,
        // and only a custom allocator would find those.
        //
        // SAFETY: NUL is valid UTF-8, so the String stays well formed.
        unsafe { self.secret.as_mut_vec().fill(0) };
    }
}

impl Ask {
    /// Finish with `answer`, or with nothing if the user cancelled.
    fn finish(&mut self, ctx: &egui::Context, answer: Option<String>) {
        if let Ok(mut slot) = self.answer.lock() {
            *slot = answer;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for Ask {
    /// The other half of `persist_window: false` above, and the half that
    /// actually carries the secret: this is what stops `egui::Memory` -- the
    /// text field's contents and its undo history with it -- from being
    /// serialised, whatever eframe's features say.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Escape cancels. The window's close button needs nothing here: eframe
        // acts on the request itself, and leaving without having stored an
        // answer is exactly what "no answer" is -- which ssh then reads as a
        // refusal rather than as an empty password.
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.finish(ctx, None);
            return;
        }

        let t = theme::of(&ctx.style().visuals);
        egui::TopBottomPanel::bottom("actions")
            .frame(
                egui::Frame::NONE
                    .fill(t.surface)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ctx, |ui| self.actions_ui(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(t.surface)
                    .inner_margin(egui::Margin::symmetric(16, 12)),
            )
            .show(ctx, |ui| self.prompt_ui(ui));
    }
}

impl Ask {
    /// What ssh asked, verbatim.
    ///
    /// Verbatim and monospace because the words are not ours to improve on:
    /// the host name, the key type and the fingerprint in a host-key question
    /// are what the user is being asked to check character by character, and
    /// a reflowed or restyled copy of a security question invites the habit
    /// of not reading it.
    fn prompt_ui(&mut self, ui: &mut egui::Ui) {
        let t = theme::of(ui.visuals());
        egui::ScrollArea::vertical()
            .max_height(ui.available_height())
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&self.prompt).monospace().color(t.text))
                        .wrap(),
                );
                if self.kind == Kind::Secret {
                    ui.add_space(2.0 * theme::UNIT);
                    let field = ui.add_sized(
                        [ui.available_width(), theme::CONTROL_HEIGHT],
                        egui::TextEdit::singleline(&mut self.secret)
                            .password(true)
                            .font(egui::TextStyle::Monospace),
                    );
                    if !self.focused {
                        field.request_focus();
                        self.focused = true;
                    }
                    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let secret = std::mem::take(&mut self.secret);
                        self.finish(ui.ctx(), Some(secret));
                    }
                }
            });
    }

    /// The buttons, which are the whole reason the prompt is classified.
    fn actions_ui(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match self.kind {
                // "No" first, and therefore rightmost, because the safe
                // answer to "the authenticity of this host can't be
                // established" is no. Nothing here is a default action: a
                // host key is accepted by choosing to accept it.
                Kind::Confirm => {
                    if ui.button("No").clicked() {
                        self.finish(ui.ctx(), Some(NO.to_string()));
                    }
                    if ui.button("Yes").clicked() {
                        self.finish(ui.ctx(), Some(YES.to_string()));
                    }
                }
                Kind::Secret => {
                    if ui.button("OK").clicked() {
                        let secret = std::mem::take(&mut self.secret);
                        self.finish(ui.ctx(), Some(secret));
                    }
                    if ui.button("Cancel").clicked() {
                        self.finish(ui.ctx(), None);
                    }
                }
            }
            let t = theme::of(ui.visuals());
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                // Said out loud because the alternative -- a checkbox nobody
                // ships without -- is the thing this deliberately does not do.
                ui.label(
                    egui::RichText::new("Nothing typed here is saved.")
                        .small()
                        .color(t.text_dim),
                );
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact text OpenSSH puts up for a host it has not seen. Getting a
    /// password box here instead of Yes/No is the failure that would make
    /// this feature worse than not having it.
    const HOST_KEY: &str = "The authenticity of host 'gpu-01 (10.0.0.5)' can't be established.\n\
         ED25519 key fingerprint is SHA256:HrxE0lmMGCsCJ8bXBRr1lS3xhVwGhy8gRuKfEEXAMPLE.\n\
         This key is not known by any other names.\n\
         Are you sure you want to continue connecting (yes/no/[fingerprint])? ";

    #[test]
    fn a_host_key_question_is_a_yes_or_no() {
        assert_eq!(classify(HOST_KEY), Kind::Confirm);
        // Older OpenSSH, without the fingerprint option.
        assert_eq!(
            classify("Are you sure you want to continue connecting (yes/no)? "),
            Kind::Confirm
        );
        // And the reprompt after an answer it did not understand.
        assert_eq!(
            classify("Please type 'yes', 'no' or the fingerprint: "),
            Kind::Confirm
        );
    }

    #[test]
    fn everything_else_is_something_to_type() {
        for prompt in [
            "alice@gpu-01's password: ",
            "Enter passphrase for key '/home/alice/.ssh/id_ed25519': ",
            "(alice@gpu-01) Verification code: ",
            "Duo two-factor login for alice\nPasscode or option (1-3): ",
        ] {
            assert_eq!(classify(prompt), Kind::Secret, "{prompt:?}");
        }
    }

    #[test]
    fn a_host_key_question_opens_taller_than_a_passphrase_one() {
        // Five lines against one; one height for both leaves the important
        // question scrolled out of sight.
        assert!(height_for(HOST_KEY, Kind::Confirm) > height_for("Password: ", Kind::Secret));
    }

    /// Only an invocation that looks exactly like ssh's is treated as one.
    ///
    /// The marker is a parameter rather than a real environment variable
    /// because `set_var` mutates the whole process: libtest runs these seven
    /// tests on several threads at once, and a `setenv` racing another
    /// thread's `getenv` is a data race in C, not just in Rust's newer rules
    /// about it.
    #[test]
    fn the_helper_is_recognised_by_marker_and_shape() {
        let argv = |args: &[&str]| -> Vec<OsString> {
            args.iter().map(OsString::from).collect::<Vec<_>>()
        };
        let marked = || Some(OsString::from("1"));

        // Without the marker this is just someone connecting to a host.
        assert_eq!(
            helper_prompt(None, argv(&["lynxrdp", "alice@gpu-01"])),
            None
        );

        assert_eq!(
            helper_prompt(marked(), argv(&["lynxrdp", "Password: "])),
            Some("Password: ".to_string())
        );
        // ssh passes exactly one argument, and never an option or an empty
        // one. Anything else is a real command line that happens to have
        // inherited the marker, and must open the client, not a dialog.
        assert_eq!(helper_prompt(marked(), argv(&["lynxrdp"])), None);
        assert_eq!(
            helper_prompt(marked(), argv(&["lynxrdp", "host", "--fullscreen"])),
            None
        );
        assert_eq!(
            helper_prompt(marked(), argv(&["lynxrdp", "--version"])),
            None
        );
        assert_eq!(helper_prompt(marked(), argv(&["lynxrdp", "   "])), None);
    }

    #[test]
    fn ssh_is_pointed_at_this_binary_and_told_to_use_it() {
        let env = ssh_env_from(Path::new("/opt/lynxrdp/bin/lynxrdp"), None, None);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("SSH_ASKPASS"), Some("/opt/lynxrdp/bin/lynxrdp"));
        assert_eq!(get(HELPER_MARKER), Some("1"));
        // Without force, OpenSSH uses an askpass only when DISPLAY is set,
        // which on macOS it never is.
        assert_eq!(get("SSH_ASKPASS_REQUIRE"), Some("force"));
    }

    #[test]
    fn an_askpass_the_user_already_chose_is_left_alone() {
        // A desktop that ships ssh-askpass, or a line in a profile, is a
        // deliberate choice; ours is the fallback, not the override.
        let env = ssh_env_from(
            Path::new("/opt/lynxrdp/bin/lynxrdp"),
            Some(OsStr::new("/usr/bin/ssh-askpass")),
            None,
        );
        assert!(!env.iter().any(|(k, _)| k == "SSH_ASKPASS"));
        assert!(
            !env.iter().any(|(k, _)| k == HELPER_MARKER),
            "the helper marker must not travel without the program that reads it"
        );
        // The preference for asking rather than using a tty still applies.
        assert!(env.iter().any(|(k, _)| k == "SSH_ASKPASS_REQUIRE"));

        let env = ssh_env_from(
            Path::new("/opt/lynxrdp/bin/lynxrdp"),
            None,
            Some(OsStr::new("never")),
        );
        assert!(!env.iter().any(|(k, _)| k == "SSH_ASKPASS_REQUIRE"));
    }

    #[test]
    fn nothing_is_written_anywhere() {
        // The rule this module is under, asserted where someone adding a
        // "remember me" would have to delete it: no path, file or directory
        // name appears in what we hand ssh, and the only value that is not a
        // constant is our own executable.
        let env = ssh_env_from(Path::new("/opt/lynxrdp/bin/lynxrdp"), None, None);
        assert!(env.len() <= 3);
        for (key, _) in &env {
            assert!(
                matches!(key.as_str(), "SSH_ASKPASS" | "SSH_ASKPASS_REQUIRE")
                    || key == HELPER_MARKER,
                "unexpected variable {key}"
            );
        }
    }
}
