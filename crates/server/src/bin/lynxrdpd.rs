//! `lynxrdpd`: the LynxRDP daemon.
//!
//! Listens on loopback only, identifies the local user behind each
//! connection (the SSH tunnel endpoint), and hands the connection to that
//! user's session, starting one under a PAM login session if necessary.

#![cfg(target_os = "linux")]

use std::net::TcpListener;
use std::os::unix::io::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use lynxrdp_proto::message::reject;
use lynxrdp_server::config::{Config, DEFAULT_CONFIG_PATH};
use lynxrdp_server::daemon::manager::{HandoffPool, SessionManager};
use lynxrdp_server::daemon::supervisor::{self, SupervisorArgs};
use lynxrdp_server::daemon::{decide, poll_listeners, send_rejection, Decision};
use lynxrdp_server::peer;
use lynxrdp_server::reporting::Reporter;

/// LynxRDP daemon.
#[derive(Parser, Debug)]
#[command(name = "lynxrdpd", version, about)]
struct Args {
    /// Configuration file.
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,
    /// Print the effective configuration and exit.
    #[arg(long)]
    dump_config: bool,
    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,
    /// Run without root: only sessions for the invoking user are possible.
    #[arg(long)]
    allow_non_root: bool,
    /// Terminate all sessions started by this daemon when it exits.
    #[arg(long)]
    stop_sessions_on_exit: bool,

    // ---- internal: session supervisor mode ----
    #[arg(long, hide = true)]
    supervise: bool,
    #[arg(long, hide = true)]
    uid: Option<u32>,
    #[arg(long, hide = true)]
    gid: Option<u32>,
    #[arg(long, hide = true)]
    user: Option<String>,
    #[arg(long, hide = true)]
    home: Option<String>,
    #[arg(long, hide = true)]
    shell: Option<String>,
    #[arg(long, hide = true)]
    control_fd: Option<i32>,
    #[arg(long, hide = true)]
    log_fd: Option<i32>,
    #[arg(long, hide = true)]
    session_binary: Option<PathBuf>,
    #[arg(long, hide = true)]
    pam_service: Option<String>,
    /// Arguments for lynxrdp-session (supervisor mode).
    #[arg(last = true, hide = true)]
    session_args: Vec<String>,
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let code = match run() {
        Ok(c) => c,
        Err(e) => {
            log::error!("{e:#}");
            eprintln!("lynxrdpd: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let args = Args::parse();
    if args.supervise {
        return run_supervisor(args);
    }
    let cfg = Config::load(&args.config)?;
    if args.dump_config {
        print!("{}", cfg.to_toml());
        return Ok(0);
    }
    if args.check {
        println!("configuration OK");
        return Ok(0);
    }
    let own_uid = peer::own_uid();
    if own_uid != 0 && !args.allow_non_root {
        bail!("lynxrdpd must run as root (or pass --allow-non-root to serve only your own user)");
    }
    // SAFETY: minimal async-signal-safe handlers.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let manager = Arc::new(SessionManager::new(cfg.clone())?);
    let tcp = TcpListener::bind(cfg.listen_addr())
        .with_context(|| format!("binding {}", cfg.listen_addr()))?;
    let bound = tcp.local_addr()?;
    if !bound.ip().is_loopback() {
        bail!("refusing to listen on non-loopback address {bound}");
    }
    log::info!(
        "lynxrdpd {} listening on {bound}",
        env!("CARGO_PKG_VERSION")
    );
    let unix = match &cfg.listen.unix_socket {
        Some(path) => {
            let _ = std::fs::remove_file(path);
            let l =
                UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
            // Anyone local may connect; identity comes from SO_PEERCRED.
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
            log::info!("also listening on unix socket {}", path.display());
            Some(l)
        }
        None => None,
    };
    let mut fds = vec![tcp.as_raw_fd()];
    if let Some(u) = &unix {
        fds.push(u.as_raw_fd());
    }
    tcp.set_nonblocking(true)?;
    if let Some(u) = &unix {
        u.set_nonblocking(true)?;
    }

    // Optional heartbeats to a monitoring server. Outbound only, on its own
    // thread, and dropped (which stops it) when this function returns.
    let reporter = match Reporter::start(&cfg) {
        Ok(r) => r,
        Err(e) => {
            // A bad destination is a configuration error worth shouting about,
            // but it must not stop the daemon serving sessions.
            log::error!("reporting is enabled but could not start: {e:#}");
            None
        }
    };

    // Handing a connection to a session is slow -- tens of seconds for a cold
    // start, forever for a session its owner has stopped -- so it happens on
    // these threads and not on the loop below. Accepting, identifying and
    // deciding stay here: they cost microseconds, and the `/proc/net/tcp`
    // lookup in particular has to run while the peer's socket is still in the
    // kernel's table.
    let mut pool = HandoffPool::new(Arc::clone(&manager)).context("starting handoff workers")?;

    while !SHUTDOWN.load(Ordering::SeqCst) {
        manager.reap();
        if let Some(r) = &reporter {
            r.set_sessions(manager.count());
        }
        let ready = match poll_listeners(&fds, Duration::from_secs(1)) {
            Ok(r) => r,
            Err(e) => {
                log::error!("poll failed: {e}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        match ready {
            Some(0) => match tcp.accept() {
                Ok((stream, addr)) => {
                    stream.set_nonblocking(false).ok();
                    let identity = match peer::tcp_peer(&stream) {
                        Ok(id) => id,
                        Err(e) => {
                            log::warn!("peer lookup for {addr} failed: {e}");
                            None
                        }
                    };
                    let fd = OwnedFd::from(stream);
                    handle_connection(&cfg, &pool, fd, identity, &addr.to_string(), own_uid);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => log::warn!("accept failed: {e}"),
            },
            Some(1) => {
                if let Some(u) = &unix {
                    match u.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).ok();
                            let identity = peer::unix_peer(&stream).ok();
                            let desc =
                                format!("unix socket pid {:?}", identity.and_then(|i| i.pid));
                            let fd = OwnedFd::from(stream);
                            handle_connection(&cfg, &pool, fd, identity, &desc, own_uid);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => log::warn!("unix accept failed: {e}"),
                    }
                }
            }
            _ => {}
        }
    }
    log::info!("shutting down ({} sessions running)", manager.count());
    // Before `stop_all`, always. A worker part-way through a spawn would
    // otherwise insert its record after the map had been drained, and that
    // session would outlive the daemon that was told to end it.
    pool.shutdown();
    if args.stop_sessions_on_exit {
        manager.stop_all();
    }
    if let Some(path) = &cfg.listen.unix_socket {
        let _ = std::fs::remove_file(path);
    }
    Ok(0)
}

/// Apply the access policy to one accepted connection and, if it passes, put
/// it in front of a handoff worker.
///
/// The descriptor arrives owned and stays owned for exactly as long as this
/// function or the pool needs it: `send_rejection` only borrows, `submit`
/// takes ownership and hands it back inside `Busy` when it will not take the
/// work. Nothing here closes anything by hand -- the previous version closed a
/// raw descriptor on every path, which is precisely the bookkeeping that stops
/// being possible once the connection can outlive this call.
fn handle_connection(
    cfg: &Config,
    pool: &HandoffPool,
    fd: OwnedFd,
    identity: Option<peer::PeerIdentity>,
    desc: &str,
    own_uid: u32,
) {
    match decide(cfg, identity) {
        Decision::Reject(code, reason) => {
            log::warn!("rejecting {desc}: {reason}");
            send_rejection(fd.as_fd(), code, &reason);
        }
        Decision::Accept(user) => {
            if own_uid != 0 && user.uid != own_uid {
                let reason = format!(
                    "daemon is not running as root; cannot start a session for {}",
                    user.name
                );
                log::warn!("rejecting {desc}: {reason}");
                send_rejection(fd.as_fd(), reject::UNAVAILABLE, &reason);
                return;
            }
            log::info!(
                "connection {desc} identified as {} (uid {})",
                user.name,
                user.uid
            );
            if let Err(busy) = pool.submit(fd, user, desc.to_string()) {
                // Being refused here is a load or abuse signal rather than a
                // configuration mistake, so it is logged as a warning and the
                // client is told, which is more than the close alone says.
                log::warn!("refusing {desc}: {}", busy.reason);
                send_rejection(busy.fd.as_fd(), reject::UNAVAILABLE, &busy.reason);
            }
        }
    }
}

fn run_supervisor(args: Args) -> Result<i32> {
    let need = |name: &str, v: Option<String>| {
        v.ok_or_else(|| anyhow::anyhow!("--supervise requires --{name}"))
    };
    let sup = SupervisorArgs {
        uid: args.uid.context("--uid")?,
        gid: args.gid.context("--gid")?,
        user: need("user", args.user)?,
        home: need("home", args.home)?,
        shell: args.shell.unwrap_or_else(|| "/bin/sh".into()),
        pam_service: args.pam_service,
        control_fd: args.control_fd.context("--control-fd")?,
        log_fd: args.log_fd.context("--log-fd")?,
        session_binary: args.session_binary.context("--session-binary")?,
        session_args: args.session_args,
    };
    supervisor::run(sup)
}
