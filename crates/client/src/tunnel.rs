//! SSH port forwarding.
//!
//! The client never talks to the server directly. It runs the system `ssh`
//! binary with a local forward to the server's loopback port and connects to
//! the local end. Using the real OpenSSH client means the user's keys, agent,
//! `known_hosts`, `~/.ssh/config`, hardware tokens and multi-factor prompts
//! all work exactly as they do for a shell login.
//!
//! The *local* end of that forward is a Unix socket wherever ssh can bind one
//! ([`LocalAddr`] says why at length) and a loopback TCP port only on Windows,
//! which has no unix-socket forwards.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::Path;

/// What to forward to on the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteTarget {
    /// `127.0.0.1:<port>` on the remote host.
    Port(u16),
    /// A Unix socket path on the remote host.
    Socket(String),
}

/// Where the local end of the forward listens.
///
/// A Unix socket wherever one can be had, because a loopback TCP port cannot
/// be *owned*. Choosing one means binding `127.0.0.1:0`, reading the number
/// back and closing the listener, and ssh does not bind the forward until
/// authentication has succeeded -- so the port belongs to nobody for the whole
/// authentication window, which is as long as the user takes to answer an MFA
/// prompt. Any local process that grabs it in that window is handed the
/// `ClientHello`, every keystroke and the clipboard, and neither end can tell.
/// Even after ssh has bound it, a loopback port is reachable by every process
/// of every user on the machine; a socket in a `0700` directory is reachable
/// only by this user.
///
/// Windows OpenSSH does not implement unix-socket forwards, so a port is all
/// there is there, and `--local-port` asks for one explicitly everywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalAddr {
    /// A loopback TCP port.
    Port(u16),
    /// A Unix socket, in a directory only this user can enter.
    #[cfg(unix)]
    Socket(PathBuf),
}

impl LocalAddr {
    /// The listen half of an `ssh -L` specification.
    ///
    /// OpenSSH recognises a path by the `/` in it and splits the rest of the
    /// specification on `:`, which is what [`socket_path_problem`] is checking
    /// against.
    fn listen_spec(&self) -> String {
        match self {
            Self::Port(p) => format!("127.0.0.1:{p}"),
            #[cfg(unix)]
            Self::Socket(path) => path.display().to_string(),
        }
    }
}

impl std::fmt::Display for LocalAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Port(p) => write!(f, "127.0.0.1:{p}"),
            #[cfg(unix)]
            Self::Socket(path) => write!(f, "{}", path.display()),
        }
    }
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
    /// Local TCP port to bind. `0` means "choose the local end", which is a
    /// Unix socket on unix and a free port on Windows; anything else is an
    /// explicit request for that TCP port.
    pub local_port: u16,
    /// `ssh` executable.
    pub ssh_program: String,
    /// Extra raw arguments passed before the destination.
    pub extra_args: Vec<String>,
    /// Environment to add to the `ssh` process.
    ///
    /// This is how the GUI askpass reaches ssh (`SSH_ASKPASS` and friends).
    /// It is set on the child rather than on ourselves because it must not
    /// leak into anything else this process starts, and because a session
    /// started from a terminal must keep prompting on that terminal.
    pub env: Vec<(String, String)>,
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
            env: Vec::new(),
        }
    }
}

impl TunnelConfig {
    /// Build the argument list for `ssh` given the chosen local end.
    pub fn args(&self, local: &LocalAddr) -> Vec<String> {
        // Note: ClearAllForwardings must NOT be set here. OpenSSH applies it
        // to command-line forwardings too, which would silently discard the
        // -L below and the tunnel would never come up.
        let mut a = vec!["-N".to_string()];
        // ExitOnForwardFailure is load-bearing rather than a preference: the
        // readiness probe below concludes the tunnel is up when the local end
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
        a.push(format!("{}:{target}", local.listen_spec()));
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

// ---- the local end, once connected -----------------------------------

/// A connection to the local end of the tunnel.
///
/// Two kinds because the local end is two kinds; everything above this either
/// reads and writes bytes or sets a timeout, and both do all of that.
#[derive(Debug)]
pub enum LocalStream {
    /// Connected to a loopback TCP port.
    Tcp(TcpStream),
    /// Connected to the Unix socket in the tunnel's private directory.
    #[cfg(unix)]
    Unix(UnixStream),
}

impl LocalStream {
    /// Duplicate the handle, as `TcpStream::try_clone` does.
    pub fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            Self::Tcp(s) => s.try_clone().map(Self::Tcp),
            #[cfg(unix)]
            Self::Unix(s) => s.try_clone().map(Self::Unix),
        }
    }

    /// Disable Nagle. Meaningless on a Unix socket, which has no Nagle to
    /// disable, so it succeeds there rather than reporting a failure a caller
    /// would have to know to ignore.
    pub fn set_nodelay(&self, on: bool) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_nodelay(on),
            #[cfg(unix)]
            Self::Unix(_) => Ok(()),
        }
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_read_timeout(d),
            #[cfg(unix)]
            Self::Unix(s) => s.set_read_timeout(d),
        }
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_write_timeout(d),
            #[cfg(unix)]
            Self::Unix(s) => s.set_write_timeout(d),
        }
    }

    /// Shut both directions down, waking a blocked reader.
    pub fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.shutdown(how),
            #[cfg(unix)]
            Self::Unix(s) => s.shutdown(how),
        }
    }

    /// The stream as a `TcpStream`, whatever kind it actually is.
    ///
    /// A bridge, and a deliberately temporary one. `connection::Client` holds
    /// a `TcpStream` and this enum is what it should hold instead; until it
    /// does, the Unix case has to arrive as a `TcpStream` or it cannot arrive
    /// at all. std's `From<OwnedFd> for TcpStream` does not check the address
    /// family, and every operation `Client` performs on the handle --
    /// `set_nodelay` (whose failure it already discards), `SO_RCVTIMEO`,
    /// `SO_SNDTIMEO`, `dup`, `read`, `write`, `shutdown` -- is either
    /// fd-level or `SOL_SOCKET`-level and works unchanged on `AF_UNIX`.
    /// `the_bridged_stream_supports_what_the_client_does` writes that list
    /// down and checks it, which is as much as a test on this side of the
    /// boundary can do: a `peer_addr()` added to `Client` would fail at run
    /// time and nowhere else. That is the reason this is meant to be
    /// short-lived rather than a permanent convenience.
    #[cfg(unix)]
    pub fn into_tcp(self) -> TcpStream {
        match self {
            Self::Tcp(s) => s,
            Self::Unix(s) => TcpStream::from(std::os::fd::OwnedFd::from(s)),
        }
    }

    /// On Windows the local end is always a TCP port, so this is the identity.
    #[cfg(not(unix))]
    pub fn into_tcp(self) -> TcpStream {
        match self {
            Self::Tcp(s) => s,
        }
    }
}

impl std::io::Read for LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            Self::Unix(s) => s.read(buf),
        }
    }
}

impl std::io::Write for LocalStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            Self::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
        }
    }
}

// Both std streams read and write through a shared reference, and
// `connection::Client` relies on that: it writes through `&stream` while a
// reader thread holds a clone.
impl std::io::Read for &LocalStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            LocalStream::Tcp(s) => (&*s).read(buf),
            #[cfg(unix)]
            LocalStream::Unix(s) => (&*s).read(buf),
        }
    }
}

impl std::io::Write for &LocalStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            LocalStream::Tcp(s) => (&*s).write(buf),
            #[cfg(unix)]
            LocalStream::Unix(s) => (&*s).write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            LocalStream::Tcp(s) => (&*s).flush(),
            #[cfg(unix)]
            LocalStream::Unix(s) => (&*s).flush(),
        }
    }
}

// ---- the private directory the socket lives in -----------------------

/// Longest path a socket address will hold, NUL terminator included.
///
/// Not a preference and not a guess to be checked at runtime: `bind(2)` copies
/// the path into a fixed `sun_path` array -- 104 bytes on macOS and the BSDs,
/// 108 on Linux -- and a path one byte over is refused, not truncated, by us
/// and by ssh alike. macOS is where this bites, because `TMPDIR` there is
/// already a ~49-character `/var/folders/...` path before we add anything.
#[cfg(unix)]
const SUN_PATH_MAX: usize = if cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
)) {
    104
} else {
    108
};

/// The socket's name inside its own directory. Short on purpose: every
/// character here comes off the budget above.
#[cfg(unix)]
const SOCKET_NAME: &str = "sock";

/// How many names to try in one base directory before moving to the next.
#[cfg(unix)]
const NAME_ATTEMPTS: u32 = 8;

/// A private directory holding the tunnel's local socket.
///
/// Created `0700` and owned by this user, so the socket inside it -- which is
/// the whole session, in the clear -- cannot be opened by anyone else on the
/// machine. Both go on drop: ssh unlinks the socket when it exits cleanly and
/// does not when we kill it, and either way the empty directory is ours to
/// remove.
#[cfg(unix)]
#[derive(Debug)]
struct SocketDir {
    dir: PathBuf,
    socket: PathBuf,
}

#[cfg(unix)]
impl Drop for SocketDir {
    fn drop(&mut self) {
        // Both are best effort. This runs while a tunnel is being torn down,
        // often on the way out of a failed connection, and a complaint about
        // a leftover directory would bury the error that actually matters.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Why `path` cannot be the local end of an `ssh -L`, if it cannot.
///
/// Pure, and worth a function of its own, because each of these only shows up
/// on the one machine whose directories happen to trip it -- and two of the
/// three fail *quietly*, by binding a socket somewhere other than the one this
/// then waits on forever.
///
/// Checked against OpenSSH 10.3: a `:` gives "Bad local forwarding
/// specification" (loud, at least); a `\` is taken as an escape and silently
/// removed, so ssh binds a different path from the one we watch; a relative
/// path is accepted and resolved against ssh's working directory rather than
/// ours. The length limit is a `bind(2)` limit rather than an ssh one, and it
/// is four bytes tighter on macOS -- which is also the platform whose
/// temporary directory is 49 characters before we add anything.
#[cfg(unix)]
fn socket_path_problem(path: &Path) -> Option<String> {
    let Some(text) = path.to_str() else {
        // The path reaches ssh as an argument we build from a `String`.
        return Some("the path is not valid UTF-8".into());
    };
    if !path.is_absolute() {
        return Some("a relative path would be resolved against ssh's directory, not ours".into());
    }
    if text.contains(':') {
        return Some("a ':' in the path is where ssh splits a forward specification".into());
    }
    if text.contains('\\') {
        return Some("ssh strips a '\\' from a forward specification as an escape".into());
    }
    if text.len() >= SUN_PATH_MAX {
        return Some(format!(
            "{} bytes is longer than the {} a socket address holds on this platform",
            text.len(),
            SUN_PATH_MAX - 1
        ));
    }
    None
}

/// Directories to try to put the socket directory in, best first.
///
/// `XDG_RUNTIME_DIR` first because the system already keeps it at `0700`, per
/// user, and cleans it up at logout -- and because it is short, which is the
/// scarce resource here. `/tmp` last and unconditionally: it is world
/// writable, but it is sticky, so nobody else can rename or remove the `0700`
/// directory we create in it, and it is the one candidate whose length is
/// known in advance to fit.
#[cfg(unix)]
fn socket_bases() -> Vec<PathBuf> {
    let mut bases = Vec::new();
    for var in ["XDG_RUNTIME_DIR", "TMPDIR"] {
        if let Some(value) = std::env::var_os(var) {
            if !value.is_empty() {
                bases.push(PathBuf::from(value));
            }
        }
    }
    bases.push(PathBuf::from("/tmp"));
    bases
}

/// A name unlikely to collide with another tunnel's, in six hex digits.
///
/// The pid alone is not enough: pids are recycled, and one process opens
/// several tunnels. The counter separates tunnels within a process and the
/// clock separates processes that share a pid over time.
#[cfg(unix)]
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{:06x}", (nanos ^ (n << 20)) & 0xff_ffff)
}

/// Create a `0700` directory for the tunnel's socket.
///
/// `create` rather than `create_dir_all`: `mkdir(2)` fails if anything is
/// already at the name, symlink included, so a unique name plus this call is
/// atomic against someone in `/tmp` trying to point it somewhere else.
#[cfg(unix)]
fn create_socket_dir() -> Result<SocketDir> {
    create_socket_dir_in(&socket_bases())
}

/// The half of [`create_socket_dir`] that does not read the environment.
///
/// Separate so the fallback can be tested: which base is usable is decided by
/// the machine the client happens to be on -- a Mac's `TMPDIR` is long enough
/// to matter and a Linux runner's may not exist at all -- and that is exactly
/// the behaviour that must not be discovered in the field.
#[cfg(unix)]
fn create_socket_dir_in(bases: &[PathBuf]) -> Result<SocketDir> {
    use std::os::unix::fs::DirBuilderExt;

    let mut refused: Vec<String> = Vec::new();
    for base in bases {
        for _ in 0..NAME_ATTEMPTS {
            let dir = base.join(format!(
                "lynxrdp-{}-{}",
                std::process::id(),
                unique_suffix()
            ));
            let socket = dir.join(SOCKET_NAME);
            if let Some(problem) = socket_path_problem(&socket) {
                // A different name in the same base would be the same length,
                // so there is nothing to retry here.
                refused.push(format!("{}: {problem}", base.display()));
                break;
            }
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(SocketDir { dir, socket }),
                // Somebody else has the name; the next suffix will differ.
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    refused.push(format!("{}: {e}", base.display()));
                    break;
                }
            }
        }
    }
    if refused.is_empty() {
        // Only reachable if every name tried was already taken, in every
        // base. Saying so beats an error with an empty parenthesis in it.
        refused.push(format!(
            "{NAME_ATTEMPTS} names were taken in every directory"
        ));
    }
    bail!(
        "could not create a private directory for the tunnel's socket ({}); \
         --local-port <port> falls back to a loopback TCP port, which every \
         other process on this machine can also reach",
        refused.join("; ")
    )
}

// ---- the tunnel ------------------------------------------------------

/// A running SSH tunnel; killed on drop.
pub struct Tunnel {
    child: Child,
    local: LocalAddr,
    /// The connection the readiness check made, kept for the real client to
    /// use rather than thrown away. See `take_stream`.
    probe: Option<LocalStream>,
    /// Held only to be dropped -- hence the name, which is also what keeps
    /// the dead-code lint from asking for a reader that would have nothing to
    /// do. Its drop removes the socket and the directory, and it happens after
    /// `Tunnel`'s own `Drop::drop` has waited for ssh, so nothing can recreate
    /// the socket behind us.
    #[cfg(unix)]
    _socket_dir: Option<SocketDir>,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Tunnel(pid {}, {})", self.child.id(), self.local)
    }
}

impl Tunnel {
    /// Start `ssh` and wait until the local forward accepts connections.
    ///
    /// SSH may prompt for a password or passphrase while this waits -- on the
    /// terminal for a command-line session, in a window for one started from
    /// the connection manager -- so `timeout` is generous and bounds the whole
    /// wait rather than one attempt.
    pub fn open(cfg: &TunnelConfig, timeout: Duration) -> Result<Self> {
        if cfg.destination.is_empty() {
            bail!("no SSH destination given");
        }
        // Underscored because on Windows there is no directory to keep: the
        // local end is a port and the type of the second half is uninhabited.
        let (local, _socket_dir) = choose_local_end(cfg)?;
        let args = cfg.args(&local);
        log::info!("starting tunnel: {} {}", cfg.ssh_program, args.join(" "));
        let mut command = Command::new(&cfg.ssh_program);
        command
            .args(&args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        for (k, v) in &cfg.env {
            command.env(k, v);
        }
        let mut child = command.spawn().with_context(|| {
            format!(
                "could not run '{}'; is an OpenSSH client installed?",
                cfg.ssh_program
            )
        })?;
        let deadline = Instant::now() + timeout;
        let probe = loop {
            if let Some(status) = child.try_wait()? {
                bail!("ssh exited before the tunnel came up ({status})");
            }
            if let Some(stream) = connect_local(&local) {
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
                break Some(stream);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                bail!("timed out waiting for the SSH tunnel");
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        log::info!("tunnel ready on {local}");
        Ok(Self {
            child,
            local,
            probe,
            #[cfg(unix)]
            _socket_dir,
        })
    }

    /// Take the connection the readiness check already established.
    ///
    /// Callers should prefer this over dialling the local end again: the far
    /// end treats every connection as a real client, so a second one replaces
    /// the session the first just attached to. On a Unix local end there is no
    /// address to dial anyway. Returns `None` only if it has already been
    /// taken.
    pub fn take_stream(&mut self) -> Option<LocalStream> {
        self.probe.take()
    }

    /// Where the local end of the tunnel is.
    pub fn local(&self) -> &LocalAddr {
        &self.local
    }

    /// The local end as a socket address, when it has one.
    ///
    /// `None` for a Unix socket, which is the usual case on unix: there is no
    /// address to connect to and nothing sensible to substitute.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.local {
            LocalAddr::Port(p) => Some(([127, 0, 0, 1], *p).into()),
            #[cfg(unix)]
            LocalAddr::Socket(_) => None,
        }
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
        // Before the fields drop, so ssh is gone by the time the socket
        // directory is removed and cannot recreate the socket behind us.
        self.close();
    }
}

/// Decide where the local end lives, creating a directory for it if needed.
///
/// A non-zero `local_port` is an explicit request for a TCP port and is
/// honoured; everything else prefers a socket.
fn choose_local_end(cfg: &TunnelConfig) -> Result<(LocalAddr, Option<SocketDirOpt>)> {
    #[cfg(unix)]
    {
        if cfg.local_port == 0 {
            let dir = create_socket_dir()?;
            let path = dir.socket.clone();
            return Ok((LocalAddr::Socket(path), Some(dir)));
        }
        log::warn!(
            "--local-port {} puts the tunnel on a loopback TCP port; every process on this \
             machine can connect to it, and until ssh binds it any of them can take it",
            cfg.local_port
        );
        Ok((LocalAddr::Port(cfg.local_port), None))
    }
    #[cfg(not(unix))]
    {
        let port = if cfg.local_port == 0 {
            pick_free_port()?
        } else {
            cfg.local_port
        };
        Ok((LocalAddr::Port(port), None))
    }
}

/// What `choose_local_end` hands back to keep alive: a directory on unix and
/// nothing at all elsewhere, so the caller needs no `cfg` of its own.
#[cfg(unix)]
type SocketDirOpt = SocketDir;
#[cfg(not(unix))]
type SocketDirOpt = std::convert::Infallible;

/// One attempt to reach the local end. `None` means "not up yet".
fn connect_local(local: &LocalAddr) -> Option<LocalStream> {
    match local {
        LocalAddr::Port(p) => {
            let addr: SocketAddr = ([127, 0, 0, 1], *p).into();
            TcpStream::connect_timeout(&addr, Duration::from_millis(200))
                .ok()
                .map(LocalStream::Tcp)
        }
        // No timeout is needed or available: connecting to a Unix socket that
        // is listening does not block, and one that is not there fails at
        // once with ENOENT, which is the state this polls through.
        #[cfg(unix)]
        LocalAddr::Socket(path) => UnixStream::connect(path).ok().map(LocalStream::Unix),
    }
}

/// Find a free loopback TCP port.
///
/// Only for a local end that was asked for by number, and on Windows, where
/// there is no alternative. The port is closed again before ssh binds it --
/// see [`LocalAddr`] for why that window matters.
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
        let a = cfg.args(&LocalAddr::Port(40000));
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
        // -p and -i stay above the user's options so a stray Port= cannot
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
        let a = cfg.args(&LocalAddr::Port(40000));
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
        let a = cfg.args(&LocalAddr::Port(1));
        assert!(a
            .windows(2)
            .any(|w| w == ["-L", "127.0.0.1:1:/run/lynxrdp/lynxrdp.sock"]));
    }

    /// A socket local end has to reach ssh as a bare path, with no host and
    /// no port in front of it -- OpenSSH tells the two apart by the leading
    /// slash and by counting colons.
    #[cfg(unix)]
    #[test]
    fn a_socket_local_end_is_written_as_a_path() {
        let cfg = TunnelConfig {
            destination: "alice@example.org".into(),
            remote: RemoteTarget::Port(3390),
            ..Default::default()
        };
        let a = cfg.args(&LocalAddr::Socket(PathBuf::from("/tmp/lynxrdp-1-a/sock")));
        assert!(
            a.windows(2)
                .any(|w| w == ["-L", "/tmp/lynxrdp-1-a/sock:127.0.0.1:3390"]),
            "{a:?}"
        );
        // Remote socket as well: two paths, one colon, both absolute.
        let cfg = TunnelConfig {
            remote: RemoteTarget::Socket("/run/lynxrdp/lynxrdp.sock".into()),
            ..cfg
        };
        let a = cfg.args(&LocalAddr::Socket(PathBuf::from("/tmp/lynxrdp-1-a/sock")));
        assert!(
            a.windows(2)
                .any(|w| w == ["-L", "/tmp/lynxrdp-1-a/sock:/run/lynxrdp/lynxrdp.sock"]),
            "{a:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_socket_path_that_ssh_or_bind_would_refuse_is_caught_first() {
        assert_eq!(
            socket_path_problem(Path::new("/tmp/lynxrdp-1-a/sock")),
            None
        );
        // A colon is where ssh splits a forward specification: OpenSSH 10.3
        // answers this one with "Bad local forwarding specification".
        assert!(socket_path_problem(Path::new("/tmp/a:b/sock"))
            .unwrap()
            .contains("':'"));
        // These two ssh accepts and then acts on differently from what they
        // say: the backslash is stripped as an escape, and a relative path is
        // resolved against ssh's working directory. Either way it binds a
        // socket somewhere other than the one the readiness wait is watching.
        assert!(socket_path_problem(Path::new("/tmp/a\\b/sock")).is_some());
        assert!(socket_path_problem(Path::new("relative/sock")).is_some());
        // Longer than sun_path: refused rather than silently truncated to
        // some other socket's name.
        let long = PathBuf::from(format!("/tmp/{}/sock", "x".repeat(SUN_PATH_MAX)));
        assert!(socket_path_problem(&long).unwrap().contains("longer"));
        // And exactly at the limit, where the NUL no longer fits.
        let edge = PathBuf::from(format!("/{}", "x".repeat(SUN_PATH_MAX - 1)));
        assert_eq!(edge.to_str().unwrap().len(), SUN_PATH_MAX);
        assert!(socket_path_problem(&edge).is_some());
    }

    /// /tmp is always a candidate and is short, so a base directory that is
    /// too long or unusable must not be the end of the story.
    #[cfg(unix)]
    #[test]
    fn there_is_always_a_short_base_to_fall_back_to() {
        assert!(socket_bases().contains(&PathBuf::from("/tmp")));
        let base = socket_bases().pop().unwrap();
        let candidate = base.join("lynxrdp-4194304-ffffff").join(SOCKET_NAME);
        assert_eq!(socket_path_problem(&candidate), None);
    }

    /// A base that cannot hold the socket is a reason to try the next one, not
    /// a reason to fail.
    ///
    /// This is the macOS case written down: `TMPDIR` there is a
    /// `/var/folders/...` path around 49 characters before anything is added
    /// to it, and it is first in the list, so the fallback is not a rare path
    /// -- it is the one every Mac takes. A machine where `TMPDIR` is missing
    /// or unwritable takes the same route for a different reason.
    #[cfg(unix)]
    #[test]
    fn an_unusable_base_falls_through_to_the_next() {
        // A base that is itself within `sun_path` but has no room left for a
        // directory and "sock" inside it -- the shape of the macOS problem,
        // not an absurd path. Caught before anything is created, rather than
        // by ssh failing to bind a truncated name.
        let too_long = PathBuf::from(format!("/{}", "x".repeat(SUN_PATH_MAX - 11)));
        assert!(too_long.to_str().unwrap().len() < SUN_PATH_MAX);
        let missing = PathBuf::from("/nonexistent-lynxrdp-base/T");
        let dir = create_socket_dir_in(&[too_long.clone(), missing.clone(), PathBuf::from("/tmp")])
            .unwrap();
        assert!(dir.dir.starts_with("/tmp"), "{:?}", dir.dir);
        drop(dir);

        // With no usable base left it is an error -- and one that names the
        // only way a user has to get a working tunnel out of this machine.
        let err = create_socket_dir_in(&[too_long, missing])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--local-port"), "{err}");
        assert!(err.contains("longer"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_directory_is_private_and_goes_away() {
        use std::os::unix::fs::PermissionsExt;
        let dir = create_socket_dir().unwrap();
        let (path, parent) = (dir.socket.clone(), dir.dir.clone());
        let mode = std::fs::metadata(&parent).unwrap().permissions().mode();
        // Only this user: the socket inside is the whole session in the clear.
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        // Created empty. ssh's own unix_listener does not unlink first, so a
        // socket already sitting there would make the forward fail to bind.
        assert!(!path.exists());
        drop(dir);
        assert!(!parent.exists(), "the directory was left behind");
    }

    #[cfg(unix)]
    #[test]
    fn two_tunnels_do_not_pick_the_same_directory() {
        let a = create_socket_dir().unwrap();
        let b = create_socket_dir().unwrap();
        assert_ne!(a.dir, b.dir);
    }

    /// The list of operations `connection::Client` performs on the handle it
    /// is given. `LocalStream::into_tcp` hands it an `AF_UNIX` fd wearing a
    /// `TcpStream`, which works only for as long as this list holds.
    #[cfg(unix)]
    #[test]
    fn the_bridged_stream_supports_what_the_client_does() {
        use std::io::{Read, Write};

        let (a, b) = UnixStream::pair().unwrap();
        let stream = LocalStream::Unix(a).into_tcp();
        // Client::from_stream, in order.
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        (&stream).write_all(b"hello").unwrap();
        let mut clone = stream.try_clone().unwrap();
        stream.set_read_timeout(None).unwrap();

        (&b).write_all(b"world").unwrap();
        let mut buf = [0u8; 5];
        clone.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"world");

        // Client::disconnect, which is what wakes the reader thread.
        stream.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(clone.read(&mut buf).unwrap(), 0);
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

    /// A fake ssh that records its arguments and then stays alive.
    ///
    /// The socket path is chosen inside `Tunnel::open`, so a test that wants
    /// to stand in for the far end has to read it back out of the command
    /// line, which is also the only place a user would see it.
    #[cfg(unix)]
    fn fake_ssh(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fakessh.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_comes_up_when_port_opens() {
        // A real listener stands in for the far end of the forward, so
        // Tunnel::open's readiness wait sees the port accept connections. The
        // fake "ssh" only has to stay alive and ignore its arguments, which a
        // tiny /bin/sh + sleep script does portably (no python required).
        let dir = tempfile::tempdir().unwrap();
        let script = fake_ssh(dir.path(), "exec sleep 30");

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
            // Non-zero: an explicit request for the old TCP local end.
            local_port: port,
            ..Default::default()
        };
        let mut t = Tunnel::open(&cfg, Duration::from_secs(10)).unwrap();
        assert!(t.is_alive());
        assert_eq!(t.local_addr().unwrap().port(), port);
        assert!(TcpStream::connect(t.local_addr().unwrap()).is_ok());
        t.close();
        assert!(!t.is_alive());
    }

    /// `TunnelConfig::env` is the whole delivery mechanism for the GUI
    /// askpass: `SSH_ASKPASS` and its two companions reach ssh this way and no
    /// other. It is one loop over `Command::env`, and if it were dropped the
    /// only symptom would be a session that fails to authenticate on a machine
    /// nobody is testing on -- so the child is asked what it actually got.
    #[cfg(unix)]
    #[test]
    fn the_environment_reaches_the_ssh_child() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("env");
        let script = fake_ssh(
            dir.path(),
            &format!(
                "printf '%s' \"$SSH_ASKPASS\" > {}\nexec sleep 30",
                record.display()
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let cfg = TunnelConfig {
            destination: "x".into(),
            ssh_program: script.display().to_string(),
            local_port: port,
            env: vec![("SSH_ASKPASS".into(), "/opt/lynxrdp/bin/lynxrdp".into())],
            ..Default::default()
        };
        // The test holds the listener, so `open` can return before the script
        // has written the file; both are only milliseconds, but neither is
        // ordered against the other.
        let mut t = Tunnel::open(&cfg, Duration::from_secs(10)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let seen = loop {
            match std::fs::read_to_string(&record) {
                Ok(s) if !s.is_empty() => break s,
                _ if Instant::now() > deadline => panic!("the fake ssh never recorded its env"),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        assert_eq!(seen, "/opt/lynxrdp/bin/lynxrdp");
        t.close();
    }

    /// The whole point of B5: with no port asked for, the local end is a Unix
    /// socket, nothing is ever bound on loopback, and the socket and its
    /// directory are gone when the tunnel is.
    #[cfg(unix)]
    #[test]
    fn the_default_local_end_is_a_private_socket() {
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv");
        let script = fake_ssh(
            dir.path(),
            &format!("echo \"$@\" > {}\nexec sleep 30", record.display()),
        );
        let cfg = TunnelConfig {
            destination: "x".into(),
            ssh_program: script.display().to_string(),
            ..Default::default()
        };

        // Tunnel::open blocks until the socket answers, and only this test
        // knows how to make it answer, so the two have to run side by side.
        let opening = std::thread::spawn(move || Tunnel::open(&cfg, Duration::from_secs(20)));

        let mut listener = None;
        for _ in 0..200 {
            if let Some(path) = std::fs::read_to_string(&record)
                .ok()
                .and_then(|argv| listen_path_of(&argv))
            {
                listener = Some(UnixListener::bind(&path).unwrap());
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _listener = listener.expect("the fake ssh never recorded a -L argument");

        let mut tunnel = opening.join().unwrap().unwrap();
        let LocalAddr::Socket(path) = tunnel.local().clone() else {
            panic!("the local end was not a socket: {:?}", tunnel.local());
        };
        let parent = path.parent().unwrap().to_path_buf();
        assert!(path.exists());
        assert_eq!(tunnel.local_addr(), None, "there is no port to hand out");
        // The readiness connection is real and is handed on rather than
        // dropped, exactly as on the TCP path.
        assert!(matches!(tunnel.take_stream(), Some(LocalStream::Unix(_))));
        assert!(tunnel.take_stream().is_none());

        drop(tunnel);
        assert!(!path.exists(), "the socket outlived the tunnel");
        assert!(!parent.exists(), "the directory outlived the tunnel");
    }

    /// The listen half of the `-L` argument in a recorded command line.
    #[cfg(unix)]
    fn listen_path_of(argv: &str) -> Option<PathBuf> {
        let mut words = argv.split_whitespace();
        while let Some(word) = words.next() {
            if word == "-L" {
                let spec = words.next()?;
                return Some(PathBuf::from(spec.split(':').next()?));
            }
        }
        None
    }
}
