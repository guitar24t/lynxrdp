//! End-to-end test of the SSH tunnel plus the protocol client against a real
//! `lynxrdp-session`, tunnelled through a throwaway `sshd`.
//!
//! Skipped unless `sshd`, `ssh` and `Xvfb` are available and a loopback SSH
//! login for the current user with a generated key works -- unless
//! `LYNXRDP_REQUIRE_E2E` is set, which makes any of those missing a failure.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use lynxrdp_client::connection::{Client, ClientEvent, ConnectOptions};
use lynxrdp_client::tunnel::{RemoteTarget, Tunnel, TunnelConfig};

mod common;
use common::{have, skip_unless, ChildGuard};

/// A port that was free a moment ago.
///
/// Only for sshd, which has to be told its port in a config file and cannot
/// be asked for one; the number can be taken by another test between the
/// release here and sshd's bind, in which case sshd fails to start and the
/// suite skips. The session is not started this way -- it binds port 0 and
/// reports what it got.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| {
            String::from_utf8_lossy(&Command::new("id").arg("-un").output().unwrap().stdout)
                .trim()
                .to_string()
        })
}

/// A private sshd on loopback allowing key auth for the current user only.
struct Sshd {
    /// Held for its drop, which stops the sshd; first, so the directory it
    /// runs from is still there when it gets the signal.
    _child: ChildGuard,
    port: u16,
    key: PathBuf,
    _dir: tempfile::TempDir,
}

impl Sshd {
    fn start() -> Option<Self> {
        if !have("sshd") || !have("ssh") || !have("ssh-keygen") {
            return None;
        }
        // sshd needs its compiled-in privilege separation directory.
        let _ = std::fs::create_dir_all("/run/sshd");
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let hostkey = d.join("host_ed25519");
        let userkey = d.join("id_ed25519");
        for path in [&hostkey, &userkey] {
            let ok = Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-N", "", "-q", "-f"])
                .arg(path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                return None;
            }
        }
        let authorized = d.join("authorized_keys");
        std::fs::copy(userkey.with_extension("pub"), &authorized).unwrap();
        set_mode(&authorized, 0o600);
        set_mode(&userkey, 0o600);
        let user = current_user();
        let port = free_port();
        let cfg = d.join("sshd_config");
        let mut f = std::fs::File::create(&cfg).unwrap();
        writeln!(
            f,
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {hk}\nPidFile {pid}\n\
             AuthorizedKeysFile {ak}\nPasswordAuthentication no\nPubkeyAuthentication yes\n\
             UsePAM no\nAllowUsers {user}\nStrictModes no\nAllowTcpForwarding yes\n\
             PermitOpen any\nLogLevel ERROR\nSetEnv PATH={bin}:/usr/bin:/bin",
            bin = std::path::Path::new(env!("CARGO_BIN_EXE_lynxrdp-session"))
                .parent()
                .unwrap()
                .display(),
            hk = hostkey.display(),
            pid = d.join("sshd.pid").display(),
            ak = authorized.display(),
        )
        .unwrap();
        drop(f);
        let sshd_bin = if std::path::Path::new("/usr/sbin/sshd").exists() {
            "/usr/sbin/sshd"
        } else {
            "sshd"
        };
        // Guarded at once: the readiness loop below gives up on a slow sshd
        // by returning `None`, which would otherwise leave it running in a
        // directory this function has just deleted, and print SKIP.
        let child = ChildGuard::new(
            Command::new(sshd_bin)
                .arg("-D")
                .arg("-f")
                .arg(&cfg)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?,
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(Self {
            _child: child,
            port,
            key: userkey,
            _dir: dir,
        })
    }
}

fn start_session() -> (ChildGuard, u16) {
    // Port 0, and the bound address read back from the session: a port
    // picked here by binding and releasing it can be handed to another test
    // in the meantime.
    let mut child = ChildGuard::new(
        Command::new(env!("CARGO_BIN_EXE_lynxrdp-session"))
            .args(["--listen", "127.0.0.1:0"])
            .args(["--width", "400", "--height", "300", "--startwm", "none"])
            .arg("--print-display")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start session"),
    );
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(line.starts_with(':'), "expected display, got {line:?}");
    line.clear();
    stdout.read_line(&mut line).unwrap();
    let port = SocketAddr::from_str(line.trim())
        .unwrap_or_else(|e| panic!("expected the listen address, got {line:?}: {e}"))
        .port();
    (child, port)
}

#[test]
fn connects_through_a_real_ssh_tunnel() {
    if skip_unless(have("Xvfb"), "Xvfb not installed") {
        return;
    }
    let Some(sshd) = Sshd::start() else {
        if skip_unless(false, "could not start a private sshd") {
            return;
        }
        unreachable!("skip_unless(false) either panics or returns true");
    };
    let (mut session, session_port) = start_session();

    let cfg = TunnelConfig {
        destination: format!("{}@127.0.0.1", current_user()),
        ssh_port: Some(sshd.port),
        identity: Some(sshd.key.clone()),
        options: vec![
            "StrictHostKeyChecking=no".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "BatchMode=yes".into(),
            "IdentitiesOnly=yes".into(),
        ],
        remote: RemoteTarget::Port(session_port),
        ..Default::default()
    };
    let mut tunnel = match Tunnel::open(&cfg, Duration::from_secs(20)) {
        Ok(t) => t,
        Err(e) => {
            if skip_unless(false, &format!("ssh login not available here: {e}")) {
                return;
            }
            unreachable!("skip_unless(false) either panics or returns true");
        }
    };

    // Take the connection the readiness check already made rather than dialling
    // the local end again, which is what the client itself does -- and here it
    // is the only thing that can work: with no `--local-port` the local end of
    // the forward is a Unix socket in a private directory, so `local_addr()` is
    // `None` and there is no address left to connect to. That makes this the
    // one test that drives the default (socket) local end through a real ssh
    // and a real sshd.
    let stream = tunnel
        .take_stream()
        .expect("the tunnel came up without a connection");
    let mut client = Client::from_stream(stream.into_tcp(), &ConnectOptions::default(), None)
        .expect("connect through tunnel");
    assert_eq!(client.size(), (400, 300));

    // Management uses another SSH channel and must not take over this desktop.
    let records = lynxrdp_client::remote_sessions::run(&cfg, None).expect("list sessions over SSH");
    let record = records
        .iter()
        .find(|r| r.pid == session.id())
        .expect("running desktop listed");
    assert!(
        lynxrdp_client::remote_sessions::run(&cfg, Some((record.pid, record.started + 1))).is_err()
    );

    // A heartbeat proves the full path (client -> ssh -> sshd -> session and
    // back) carries protocol traffic in both directions.
    client.ping().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rtt = false;
    while Instant::now() < deadline {
        if let Some(ClientEvent::Rtt(_)) = client.poll_event(Duration::from_millis(200)).unwrap() {
            rtt = true;
            break;
        }
    }
    assert!(rtt, "no round-trip through the tunnel");

    lynxrdp_client::remote_sessions::run(&cfg, Some((record.pid, record.started)))
        .expect("terminate desktop over SSH");
    let deadline = Instant::now() + Duration::from_secs(10);
    while session.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    client.disconnect("done");
    drop(tunnel);
    // The session (already exited) and the sshd go with their guards.
}
