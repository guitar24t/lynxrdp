//! An SSH helper relays prompts to the owning application's UI, never opens
//! another application. The socket lives in a private temporary directory.
use super::*;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SOCKET_ENV: &str = "LYNXRDP_ASKPASS_SOCKET";
const LIMIT: u64 = 64 * 1024;

pub fn request(path: &Path, prompt: &str) -> Option<String> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    serde_json::to_writer(&mut stream, prompt).ok()?;
    stream.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(stream)
        .take(LIMIT)
        .read_line(&mut line)
        .ok()?;
    let answer = serde_json::from_str(&line).ok()?;
    // SAFETY: zero bytes are valid UTF-8; do not retain the encoded answer.
    unsafe { line.as_mut_vec().fill(0) };
    answer
}

struct Request {
    prompt: String,
    reply: crossbeam_channel::Sender<Option<String>>,
    deadline: Instant,
}

pub struct Broker {
    _directory: tempfile::TempDir,
    pub path: PathBuf,
    listener: UnixListener,
    tx: crossbeam_channel::Sender<Request>,
    rx: crossbeam_channel::Receiver<Request>,
    current: Option<(Ask, crossbeam_channel::Sender<Option<String>>, Instant)>,
}

impl Broker {
    pub fn new() -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::Builder::new().prefix("lynxrdp-auth-").tempdir()?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let path = directory.path().join("prompt.sock");
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        let (tx, rx) = crossbeam_channel::bounded(8);
        Ok(Self {
            _directory: directory,
            path,
            listener,
            tx,
            rx,
            current: None,
        })
    }

    pub fn cancel(&mut self) -> bool {
        if let Some((_, reply, _)) = self.current.take() {
            let _ = reply.send(None);
            return true;
        }
        false
    }

    pub fn poll(&mut self) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|(_, _, deadline)| Instant::now() >= *deadline)
        {
            self.cancel();
        }
        // At most one new helper per tick; slow/malformed peers never block UI.
        if let Ok((mut stream, _)) = self.listener.accept() {
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                if BufReader::new(&mut stream)
                    .take(LIMIT)
                    .read_line(&mut line)
                    .is_err()
                {
                    return;
                }
                let Ok(prompt) = serde_json::from_str::<String>(&line) else {
                    return;
                };
                let (reply, answer) = crossbeam_channel::bounded(1);
                if tx
                    .try_send(Request {
                        prompt,
                        reply,
                        deadline: Instant::now() + Duration::from_secs(120),
                    })
                    .is_err()
                {
                    return;
                }
                if let Ok(mut answer) = answer.recv_timeout(Duration::from_secs(120)) {
                    let _ = serde_json::to_writer(&mut stream, &answer);
                    let _ = stream.write_all(b"\n");
                    if let Some(secret) = &mut answer {
                        // SAFETY: zero bytes preserve UTF-8 validity.
                        unsafe { secret.as_mut_vec().fill(0) };
                    }
                }
            });
        }
        if self.current.is_none() {
            if let Ok(request) = self.rx.try_recv() {
                let ask = Ask {
                    kind: classify(&request.prompt),
                    prompt: request.prompt,
                    secret: String::new(),
                    focused: false,
                    answer: Default::default(),
                    embedded: true,
                    completed: false,
                };
                self.current = Some((ask, request.reply, request.deadline));
                return true;
            }
        }
        false
    }

    pub fn show(&mut self, ctx: &egui::Context) {
        let Some((ask, _, _)) = self.current.as_mut() else {
            return;
        };
        egui::Modal::new(egui::Id::new("ssh-authentication")).show(ctx, |ui| {
            ui.set_width(WIDTH);
            ui.set_max_height(height_for(&ask.prompt, ask.kind));
            ui.heading("SSH authentication");
            ask.prompt_ui(ui);
            ui.add_space(12.0);
            ask.actions_ui(ui);
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                ask.finish(ctx, None);
            }
        });
        if ask.completed {
            let (ask, reply, _) = self.current.take().unwrap();
            let answer = ask.answer.lock().ok().and_then(|mut a| a.take());
            let _ = reply.send(answer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_prompts_and_cancellation_use_the_owning_ui() {
        use std::os::unix::fs::PermissionsExt;
        let mut broker = Broker::new().unwrap();
        assert_eq!(
            std::fs::metadata(broker.path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for answer in [Some("test answer".to_string()), None] {
            let path = broker.path.clone();
            let helper = std::thread::spawn(move || request(&path, "Password: "));
            let deadline = Instant::now() + Duration::from_secs(3);
            while broker.current.is_none() {
                broker.poll();
                assert!(Instant::now() < deadline, "helper prompt never arrived");
                std::thread::sleep(Duration::from_millis(5));
            }
            let ctx = egui::Context::default();
            if answer.is_some() {
                broker
                    .current
                    .as_mut()
                    .unwrap()
                    .0
                    .finish(&ctx, answer.clone());
                let output = ctx.run(Default::default(), |ctx| broker.show(ctx));
                assert!(output.viewport_output.values().all(|v| !v
                    .commands
                    .iter()
                    .any(|c| matches!(c, egui::ViewportCommand::Close))));
            } else {
                assert!(broker.cancel());
            }
            assert_eq!(helper.join().unwrap(), answer);
            assert!(broker.current.is_none());
        }
    }
}
