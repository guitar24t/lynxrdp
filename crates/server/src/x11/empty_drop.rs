//! Bounded accessibility lookup for Nautilus's non-droppable empty-state overlay.
use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(super) struct Probe {
    pub pid: u32,
    pub x: u16,
    pub y: u16,
    pub window: (i16, i16, u16, u16),
}
impl Probe {
    pub fn start(self) -> crossbeam_channel::Receiver<Option<(u16, u16)>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let _ = thread::Builder::new()
            .name("empty-folder-drop".into())
            .spawn(move || {
                let result = self.lookup();
                let _ = tx.send(result);
            });
        rx
    }
    fn lookup(self) -> Option<(u16, u16)> {
        let mut command = Command::new("python3");
        command
            .args(["-c", include_str!("empty_drop.py")])
            .args([
                self.pid.to_string(),
                self.x.to_string(),
                self.y.to_string(),
                self.window.0.to_string(),
                self.window.1.to_string(),
                self.window.2.to_string(),
                self.window.3.to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            // Standard per-user GNOME session bus. The lookup additionally
            // matches the X window's process ID and exact frame geometry.
            command.env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path=/run/user/{}/bus", unsafe { libc::geteuid() }),
            );
        }
        let mut child = command.spawn().ok()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    let output = child.wait_with_output().ok()?;
                    return serde_json::from_slice(&output.stdout).ok().flatten();
                }
                Ok(Some(_)) => return None,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_view_target_contract() {
        let status = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/test_empty_drop.py"
            ))
            .status()
            .expect("Python 3 is required for the empty-view contract tests");
        assert!(status.success());
    }
}
