//! SSH port forwarding.
//!
//! The client never talks to the server directly. It runs the system `ssh`
//! binary with a local port forward to the server's loopback port and
//! connects to the local end. Using the real OpenSSH client means the
//! user's keys, agent, `known_hosts`, `~/.ssh/config`, hardware tokens and
//! multi-factor prompts all work exactly as they do for a shell login.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// What to forward to on the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteTarget {
    /// `127.0.0.1:<port>` on the remote host.
    Port(u16),
    /// A Unix socket path on the remote host.
    Socket(String),
}

/// SSH invocation parameters.
#[derive(Clone, Debug)]
pub struct TunnelConfig {
    /// `[user@]host` (or an alias from `~/.ssh/config`).
    pub destination: String,
    /// SSH server port (`None` = default / config).
    pub ssh_port: Option<u16>,
    /// Identity file (`-i`).
    pub identity: Option<PathBuf>,
    /// Extra `-o key=value` options.
    pub options: Vec<String>,
    /// Remote target of the forward.
    pub remote: RemoteTarget,
    /// Local port to bind (`0` = pick a free one).
    pub local_port: u16,
    /// `ssh` executable.
    pub ssh_program: String,
    /// Extra raw arguments passed before the destination.
    pub extra_args: Vec<String>,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            destination: String::new(),
            ssh_port: None,
            identity: None,
            options: Vec::new(),
            remote: RemoteTarget::Port(lynxrdp_proto::DEFAULT_PORT),
            local_port: 0,
            ssh_program: "ssh".to_string(),
            extra_args: Vec::new(),
        }
    }
}

impl TunnelConfig {
    /// Build the argument list for `ssh` given the chosen local port.
    pub fn args(&self, local_port: u16) -> Vec<String> {
        // Note: ClearAllForwardings must NOT be set here. OpenSSH applies it
        // to command-line forwardings too, which would silently discard the
        // -L below and the tunnel would never come up.
        let mut a = vec!["-N".to_string()];
        // ExitOnForwardFailure is load-bearing rather than a preference: the
        // readiness probe below concludes the tunnel is up when the local port
        // accepts, and without this ssh would happily stay connected with no
        // forward at all. So it is pinned, and a user setting that collides
        // with it is refused loudly rather than silently losing.
        a.extend(["-o".to_string(), "ExitOnForwardFailure=yes".into()]);
        if let Some(p) = self.ssh_port {
            a.push("-p".into());
            a.push(p.to_string());
        }
        if let Some(i) = &self.identity {
            a.push("-i".into());
            a.push(i.display().to_string());
        }
        // The user's options come BEFORE our remaining defaults, because
        // OpenSSH takes the *first* value it sees for a keyword. With the
        // defaults first, anything typed into the launcher's ssh_options box
        // that named ServerAliveInterval, TCPKeepAlive or Compression was
        // accepted, saved, passed to ssh -- and then quietly ignored.
        //
        // -p and -i stay above them so a stray `Port=` cannot override the
        // port field of the profile it sits next to.
        for o in &self.options {
            if keyword_of(o).is_some_and(|k| k.eq_ignore_ascii_case("exitonforwardfailure")) {
                log::warn!(
                    "ignoring ssh option {o:?}: the tunnel's readiness check depends on \
                     ExitOnForwardFailure=yes"
                );
                continue;
            }
            a.push("-o".into());
            a.push(o.clone());
        }
        // Defaults last, so any of these the user set above wins. TCP keepalive
        // on the SSH connection itself, and no compression: our stream is
        // already compressed and adding zlib only adds latency.
        for (k, v) in [
            ("ServerAliveInterval", "15"),
            ("ServerAliveCountMax", "3"),
            ("TCPKeepAlive", "yes"),
            ("Compression", "no"),
        ] {
            a.push("-o".into());
            a.push(format!("{k}={v}"));
        }
        let target = match &self.remote {
            RemoteTarget::Port(p) => format!("127.0.0.1:{p}"),
            RemoteTarget::Socket(path) => path.clone(),
        };
        a.push("-L".into());
        a.push(format!("127.0.0.1:{local_port}:{target}"));
        a.extend(self.extra_args.iter().cloned());
        a.push("--".into());
        a.push(self.destination.clone());
        a
    }
}

/// The keyword part of an `ssh -o` argument, i.e. what comes before `=` or
/// whitespace. `None` when the argument has no recognisable keyword.
fn keyword_of(option: &str) -> Option<&str> {
    let k = option
        .split(['=', ' ', '\t'])
        .next()
        .map(str::trim)
        .filter(|k| !k.is_empty())?;
    Some(k)
}

/// A running SSH tunnel; killed on drop.
pub struct Tunnel {
    child: Child,
    local_addr: SocketAddr,
    /// The connection the readiness check made, kept for the real client to
    /// use rather than thrown away. See `take_stream`.
    probe: Option<TcpStream>,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tunnel(pid {}, {})", self.child.id(), self.local_addr)
    }
}

impl Tunnel {
    /// Start `ssh` and wait until the local forward accepts connections.
    ///
    /// SSH may prompt for a password or passphrase on the terminal while
    /// this waits; `timeout` bounds the total wait.
    pub fn open(cfg: &TunnelConfig, timeout: Duration) -> Result<Self> {
        if cfg.destination.is_empty() {
            bail!("no SSH destination given");
        }
        let local_port = if cfg.local_port == 0 {
            pick_free_port()?
        } else {
            cfg.local_port
        };
        let args = cfg.args(local_port);
        log::info!("starting tunnel: {} {}", cfg.ssh_program, args.join(" "));
        let mut child = Command::new(&cfg.ssh_program)
            .args(&args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| {
                format!(
                    "could not run '{}'; is an OpenSSH client installed?",
                    cfg.ssh_program
                )
            })?;
        let local_addr: SocketAddr = ([127, 0, 0, 1], local_port).into();
        let deadline = Instant::now() + timeout;
        let probe = loop {
            if let Some(status) = child.try_wait()? {
                bail!("ssh exited before the tunnel came up ({status})");
            }
            if let Ok(s) = TcpStream::connect_timeout(&local_addr, Duration::from_millis(200)) {
                // A successful connect means ssh has bound the forward, which
                // it only does after authentication succeeded.
                //
                // Keep it. This is not a probe as far as the far end is
                // concerned: it reaches lynxrdpd, which identifies the peer and
                // starts or attaches that user's session. Dropping it therefore
                // evicted a client connected from another device ("Another
                // client connected to this session") and then immediately went
                // away, and it killed an --exit-on-disconnect session outright
                // before the real client ever arrived. Handing the same
                // connection to `Client::from_stream` costs nothing and makes
                // the readiness check honest.
                break Some(s);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("timed out waiting for the SSH tunnel");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        log::info!("tunnel ready on {local_addr}");
        Ok(Self {
            child,
            local_addr,
            probe,
        })
    }

    /// Take the connection the readiness check already established.
    ///
    /// Callers should prefer this over dialling `local_addr` again: the far end
    /// treats every connection as a real client, so a second one replaces the
    /// session the first just attached to. Returns `None` only if it has
    /// already been taken.
    pub fn take_stream(&mut self) -> Option<TcpStream> {
        self.probe.take()
    }

    /// Local end of the tunnel.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether `ssh` is still running.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stop the tunnel.
    pub fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.close();
    }
}

/// Find a free loopback TCP port.
pub fn pick_free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").context("no free loopback port")?;
    Ok(l.local_addr()?.port())
}

/// Parse `[user@]host[:port]` into destination and optional port.
pub fn parse_destination(s: &str) -> Result<(String, Option<u16>)> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty destination");
    }
    // IPv6 literal in brackets: [::1]:22
    if let Some(rest) = s
        .strip_prefix('[')
        .or_else(|| s.find("@[").map(|i| &s[i + 2..]))
    {
        if let Some(end) = rest.find(']') {
            let host = &rest[..end];
            let user = s.find("@[").map(|i| &s[..i]);
            let dest = match user {
                Some(u) => format!("{u}@{host}"),
                None => host.to_string(),
            };
            let port = rest[end + 1..]
                .strip_prefix(':')
                .map(|p| p.parse::<u16>())
                .transpose()
                .context("bad port")?;
            return Ok((dest, port));
        }
        bail!("unterminated IPv6 literal");
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && !port.is_empty() => {
            let port = port
                .parse::<u16>()
                .with_context(|| format!("bad port {port:?}"))?;
            Ok((host.to_string(), Some(port)))
        }
        _ => Ok((s.to_string(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A user's ssh option must beat our default, not be silently discarded.
    ///
    /// OpenSSH takes the *first* value it sees for a keyword. The built-in
    /// defaults used to be emitted before the user's, so anything typed into
    /// the launcher's ssh_options box that named ServerAliveInterval,
    /// TCPKeepAlive or Compression was accepted, saved, passed to ssh, and then
    /// quietly ignored -- the worst kind of failure, because the setting is
    /// visibly there.
    #[test]
    fn a_user_option_wins_over_the_matching_default() {
        let cfg = TunnelConfig {
            destination: "alice@example.org".into(),
            options: vec![
                "ServerAliveInterval=60".into(),
                "Compression=yes".into(),
                // Pinned: the readiness probe depends on it, so this one is
                // refused rather than honoured.
                "ExitOnForwardFailure=no".into(),
            ],
            ..Default::default()
        };
        let a = cfg.args(40000);
        let first_index = |needle: &str| {
            a.iter()
                .position(|s| s == needle)
                .unwrap_or_else(|| panic!("{needle} missing from {a:?}"))
        };
        assert!(
            first_index("ServerAliveInterval=60") < first_index("ServerAliveInterval=15"),
            "the default was emitted first and would win: {a:?}"
        );
        assert!(
            first_index("Compression=yes") < first_index("Compression=no"),
            "the default was emitted first and would win: {a:?}"
        );
        assert!(
            !a.iter().any(|s| s == "ExitOnForwardFailure=no"),
            "a user option overrode the pinned ExitOnForwardFailure: {a:?}"
        );
        assert!(a.iter().any(|s| s == "ExitOnForwardFailure=yes"));
        // -p and -i must stay above the user's options so a stray Port= cannot
        // override the profile's own port field.
        assert!(first_index("-o") > 0);
    }

    #[test]
    fn builds_ssh_arguments() {
        let cfg = TunnelConfig {
            destination: "alice@example.org".into(),
            ssh_port: Some(2222),
            identity: Some(PathBuf::from("/k/id")),
            options: vec!["StrictHostKeyChecking=yes".into()],
            remote: RemoteTarget::Port(3390),
            ..Default::default()
        };
        let a = cfg.args(40000);
        assert_eq!(a[0], "-N");
        assert!(a.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(a.windows(2).any(|w| w == ["-i", "/k/id"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["-o", "StrictHostKeyChecking=yes"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["-L", "127.0.0.1:40000:127.0.0.1:3390"]));
        assert_eq!(&a[a.len() - 2..], ["--", "alice@example.org"]);
        let cfg = TunnelConfig {
            remote: RemoteTarget::Socket("/run/lynxrdp/lynxrdp.sock".into()),
            ..cfg
        };
        let a = cfg.args(1);
        assert!(a
            .windows(2)
            .any(|w| w == ["-L", "127.0.0.1:1:/run/lynxrdp/lynxrdp.sock"]));
    }

    #[test]
    fn parses_destinations() {
        assert_eq!(parse_destination("host").unwrap(), ("host".into(), None));
        assert_eq!(
            parse_destination("u@host:22").unwrap(),
            ("u@host".into(), Some(22))
        );
        assert_eq!(
            parse_destination("[::1]:2200").unwrap(),
            ("::1".into(), Some(2200))
        );
        assert_eq!(
            parse_destination("u@[fe80::1]").unwrap(),
            ("u@fe80::1".into(), None)
        );
        assert!(parse_destination("").is_err());
        assert!(parse_destination("h:notaport").is_err());
    }

    #[test]
    fn free_port_is_bindable() {
        let p = pick_free_port().unwrap();
        assert!(p > 0);
    }

    #[test]
    fn missing_ssh_binary_is_reported() {
        let cfg = TunnelConfig {
            destination: "nowhere".into(),
            ssh_program: "/nonexistent/ssh-binary".into(),
            ..Default::default()
        };
        let err = Tunnel::open(&cfg, Duration::from_secs(1)).unwrap_err();
        assert!(err.to_string().contains("could not run"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn failing_ssh_is_reported() {
        let cfg = TunnelConfig {
            destination: "x".into(),
            ssh_program: "false".into(),
            ..Default::default()
        };
        let err = Tunnel::open(&cfg, Duration::from_secs(5)).unwrap_err();
        assert!(err.to_string().contains("exited"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_comes_up_when_port_opens() {
        // A real listener stands in for the far end of the forward, so
        // Tunnel::open's readiness wait sees the port accept connections. The
        // fake "ssh" only has to stay alive and ignore its arguments, which a
        // tiny /bin/sh + sleep script does portably (no python required).
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fakessh.sh");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Bind first and read the port back from the listener we are holding.
        // Asking for a free port and then re-binding it leaves a window where
        // the port can be taken, which is exactly what failed on macOS.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_err() {
                    break;
                }
            }
        });

        let cfg = TunnelConfig {
            destination: "x".into(),
            ssh_program: script.display().to_string(),
            local_port: port,
            ..Default::default()
        };
        let mut t = Tunnel::open(&cfg, Duration::from_secs(10)).unwrap();
        assert!(t.is_alive());
        assert_eq!(t.local_addr().port(), port);
        assert!(TcpStream::connect(t.local_addr()).is_ok());
        t.close();
        assert!(!t.is_alive());
    }
}
