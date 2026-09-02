//! End-to-end tests for `lynxrdpd` in `--allow-non-root` mode: the daemon
//! identifies the connecting uid, spawns a supervisor and a session for it,
//! and hands the connection over. Needs Xvfb.

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use lynxrdp_client::connection::{Client, ClientEvent, ConnectOptions};

fn have(prog: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {prog}"))
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct Daemon {
    child: Child,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start(extra_toml: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cfg = format!(
            "[listen]\nport = {port}\n[access]\nmin_uid = 0\ndeny_users = []\n{extra_toml}\n[session]\ndefault_width = 320\ndefault_height = 240\nmax_width = 640\nmax_height = 480\nstartwm = \"none\"\nruntime_dir = \"{rt}\"\nlog_dir = \"{log}\"\nsession_binary = \"{bin}\"\npam_service = \"\"\n",
            rt = dir.path().join("rt").display(),
            log = dir.path().join("log").display(),
            bin = env!("CARGO_BIN_EXE_lynxrdp-session"),
        );
        let cfg_path = dir.path().join("lynxrdp.toml");
        std::fs::write(&cfg_path, cfg).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_lynxrdpd"))
            .arg("--config")
            .arg(&cfg_path)
            .arg("--allow-non-root")
            .arg("--stop-sessions-on-exit")
            .env("RUST_LOG", "debug")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start lynxrdpd");
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self {
            child,
            port,
            _dir: dir,
        }
    }

    fn connect(&self) -> anyhow::Result<Client> {
        let opts = ConnectOptions {
            timeout: Duration::from_secs(60),
            ..Default::default()
        };
        Client::connect(([127, 0, 0, 1], self.port).into(), &opts, None)
    }

    fn stop(&mut self) {
        // SAFETY: signalling our own child.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop();
    }
}

#[test]
fn daemon_spawns_session_and_hands_off() {
    if !have("Xvfb") {
        eprintln!("SKIP: Xvfb not installed");
        return;
    }
    let mut d = Daemon::start("");
    let mut c = d.connect().expect("first connection");
    assert_eq!(c.size(), (320, 240));
    assert_ne!(c.info().session_id, 0);
    let own = lynxrdp_server::daemon::users::user_by_uid(lynxrdp_server::peer::own_uid()).unwrap();
    assert_eq!(c.info().username, own.name);
    let first_id = c.info().session_id;

    // A second connection is handed to the same session and replaces the first.
    let c2 = d.connect().expect("second connection");
    assert_eq!(c2.info().session_id, first_id);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut replaced = false;
    while Instant::now() < deadline {
        if let Some(ClientEvent::Disconnected(_)) =
            c.poll_event(Duration::from_millis(200)).unwrap()
        {
            replaced = true;
            break;
        }
    }
    assert!(replaced, "first client should have been replaced");
    drop(c2);
    d.stop();
}

#[test]
fn daemon_rejects_denied_user() {
    if !have("Xvfb") {
        eprintln!("SKIP: Xvfb not installed");
        return;
    }
    let own = lynxrdp_server::daemon::users::user_by_uid(lynxrdp_server::peer::own_uid()).unwrap();
    let d = Daemon::start(&format!("allow_users = [\"nobody-else-{}\"]", own.uid));
    let err = d.connect().unwrap_err();
    assert!(err.to_string().contains("rejected"), "{err}");
}

#[test]
fn config_check_and_dump() {
    let out = Command::new(env!("CARGO_BIN_EXE_lynxrdpd"))
        .args(["--config", "/nonexistent/x.toml", "--dump-config"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("address = \"127.0.0.1\""), "{text}");
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.toml");
    std::fs::write(&bad, "[listen]\naddress = \"0.0.0.0\"\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lynxrdpd"))
        .args(["--config"])
        .arg(&bad)
        .arg("--check")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("loopback"));
}
