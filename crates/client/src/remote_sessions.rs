//! Session administration over the user's normal SSH authentication.
use crate::tunnel::{LocalAddr, TunnelConfig};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::io::{Read, Seek, SeekFrom};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Deserialize)]
pub struct SessionRecord {
    pub pid: u32,
    pub started: u64,
    pub session_id: u64,
    pub listen: Option<String>,
}
/// No shell data comes from a profile: only fixed commands and decimal IDs.
pub fn command(config: &TunnelConfig, terminate: Option<(u32, u64)>) -> Command {
    let mut cmd = Command::new(&config.ssh_program);
    cmd.args([
        "-T",
        "-o",
        "ClearAllForwardings=yes",
        "-o",
        "RemoteCommand=none",
    ]);
    let args = config.args(&LocalAddr::Port(1));
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "-N" {
            continue;
        }
        if arg == "-L" {
            iter.next();
            continue;
        }
        cmd.arg(arg);
    }
    cmd.arg(match terminate {
        Some((pid, started)) => format!("lynxrdp-session --terminate {pid} --started {started}"),
        None => "lynxrdp-session --list-sessions".into(),
    });
    for (key, value) in &config.env {
        cmd.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd
}
/// A bounded SSH command. Files keep a child from filling an undrained pipe.
pub fn run(config: &TunnelConfig, terminate: Option<(u32, u64)>) -> Result<Vec<SessionRecord>> {
    let mut output = tempfile::tempfile()?;
    let mut errors = tempfile::tempfile()?;
    let mut child = command(config, terminate)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(errors.try_clone()?)
        .spawn()
        .context("starting SSH session management")?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(120)
            || output.metadata()?.len() > 1024 * 1024
            || errors.metadata()?.len() > 1024 * 1024
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!("session management timed out or returned too much output");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if !status.success() {
        errors.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        errors.take(8192).read_to_string(&mut text)?;
        bail!("session management failed: {}", text.trim());
    }
    if terminate.is_some() {
        return Ok(Vec::new());
    }
    output.seek(SeekFrom::Start(0))?;
    let mut text = String::new();
    output.take(1024 * 1024).read_to_string(&mut text)?;
    serde_json::from_str(&text).context("reading the session list (the server may need updating)")
}
/// A remote session list belonging to one saved SSH connection.
pub struct Window {
    profile: crate::profiles::Profile,
    records: Vec<SessionRecord>,
    pending: Option<std::sync::mpsc::Receiver<Result<Vec<SessionRecord>, String>>>,
    confirm: Option<SessionRecord>,
    error: Option<String>,
    pub open: bool,
}
impl Window {
    pub fn new(profile: crate::profiles::Profile) -> Self {
        let mut window = Self {
            profile,
            records: Vec::new(),
            pending: None,
            confirm: None,
            error: None,
            open: true,
        };
        window.refresh(None);
        window
    }
    fn refresh(&mut self, terminate: Option<(u32, u64)>) {
        let config = TunnelConfig {
            destination: self.profile.destination(),
            ssh_port: self.profile.ssh_port,
            identity: self.profile.identity.clone(),
            options: self.profile.ssh_options.clone(),
            env: crate::askpass::ssh_env(),
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = (|| {
                if let Some(target) = terminate {
                    run(&config, Some(target))?;
                }
                run(&config, None)
            })();
            let _ = tx.send(result.map_err(|e: anyhow::Error| format!("{e:#}")));
        });
    }
    /// Draw and return a connection to launch when Reconnect is chosen.
    pub fn show(&mut self, ctx: &eframe::egui::Context) -> Option<crate::profiles::Profile> {
        use eframe::egui;
        if let Some(result) = self.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.pending = None;
            match result {
                Ok(records) => self.records = records,
                Err(e) => self.error = Some(e),
            }
        }
        let mut open = self.open;
        let mut reconnect = None;
        egui::Window::new(format!("Running desktops - {}", self.profile.destination())).open(&mut open).show(ctx, |ui| {
            ui.label("These are your running desktops. Refreshing does not connect to them.");
            if ui.add_enabled(self.pending.is_none(), egui::Button::new("Refresh")).clicked() { self.refresh(None); }
            if self.pending.is_some() { ui.spinner(); ctx.request_repaint_after(Duration::from_millis(100)); }
            if let Some(error) = &self.error { ui.colored_label(egui::Color32::LIGHT_RED, error); }
            if self.pending.is_none() && self.records.is_empty() { ui.label("No running desktops."); }
            for record in &self.records {
                ui.horizontal(|ui| {
                    ui.label(format!("Session {} - PID {}", record.session_id, record.pid));
                    if ui.add_enabled(self.pending.is_none(), egui::Button::new("Reconnect")).clicked() {
                        let mut profile = self.profile.clone();
                        if let Some(address) = &record.listen {
                            if let Ok(address) = address.parse::<std::net::SocketAddr>() { profile.remote_port = Some(address.port()); }
                        }
                        reconnect = Some(profile);
                    }
                    if ui.add_enabled(self.pending.is_none(), egui::Button::new("Terminate...")).clicked() { self.confirm = Some(record.clone()); }
                });
            }
            if let Some(record) = self.confirm.clone() {
                ui.separator();
                ui.label(format!("End desktop PID {}? Its applications will close and unsaved work may be lost.", record.pid));
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() { self.confirm = None; }
                    if ui.button("End desktop").clicked() { self.confirm = None; self.refresh(Some((record.pid,record.started))); }
                });
            }
        });
        self.open = open;
        reconnect
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn management_does_not_open_a_desktop_forward() {
        let cfg = TunnelConfig {
            destination: "user@host".into(),
            ..Default::default()
        };
        let cmd = command(&cfg, Some((123, 456)));
        let args: Vec<_> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|s| s == "-L" || s == "-N"));
        assert_eq!(
            args.last().unwrap(),
            "lynxrdp-session --terminate 123 --started 456"
        );
    }
}
