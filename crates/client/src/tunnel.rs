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
        let mut a = vec![
            "-N".to_string(),
            "-o".into(),
            "ExitOnForwardFailure=yes".into(),
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ];
        // TCP keepalive on the SSH connection itself and no compression: our
        // stream is already compressed and adding zlib only adds latency.
        a.extend([
            "-o".to_string(),
            "TCPKeepAlive=yes".into(),
            "-o".into(),
            "Compression=no".into(),
        ]);
        if let Some(p) = self.ssh_port {
            a.push("-p".into());
            a.push(p.to_string());
        }
        if let Some(i) = &self.identity {
            a.push("-i".into());
            a.push(i.display().to_string());
        }
        for o in &self.options {
            a.push("-o".into());
            a.push(o.clone());
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

/// A running SSH tunnel; killed on drop.
pub struct Tunnel {
    child: Child,
    local_addr: SocketAddr,
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
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("ssh exited before the tunnel came up ({status})");
            }
            if TcpStream::connect_timeout(&local_addr, Duration::from_millis(200)).is_ok() {
                // A successful connect means ssh has bound the forward, which
                // it only does after authentication succeeded.
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("timed out waiting for the SSH tunnel");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        log::info!("tunnel ready on {local_addr}");
        Ok(Self { child, local_addr })
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

        let port = pick_free_port().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
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
