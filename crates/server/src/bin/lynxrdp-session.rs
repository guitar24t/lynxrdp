//! `lynxrdp-session`: runs one user's remote desktop session.
//!
//! Normally started by `lynxrdpd`, but it can also be run directly by a
//! user ("user mode") to serve their own session on a loopback port without
//! any privileges:
//!
//! ```text
//! lynxrdp-session --listen 127.0.0.1:3390
//! ```
//!
//! Connections are only accepted from sockets owned by the same uid, which
//! is exactly what an SSH port forward for that user produces.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::net::TcpListener;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use lynxrdp_server::session::desktop::Desktop;
use lynxrdp_server::session::engine::{Core, Exit};
use lynxrdp_server::session::listener::{spawn_control_listener, spawn_tcp_listener};
use lynxrdp_server::session::xserver::{default_runtime_dir, XServer, XServerConfig};
use lynxrdp_server::session::{CoreEvent, SessionOptions};
use lynxrdp_server::x11::XDisplay;

/// Run a LynxRDP desktop session.
#[derive(Parser, Debug)]
#[command(name = "lynxrdp-session", version, about)]
struct Args {
    /// Loopback address to accept clients on directly (user mode).
    #[arg(long)]
    listen: Option<String>,
    /// Inherited fd of a Unix listening socket for handoffs from lynxrdpd.
    #[arg(long)]
    control_fd: Option<i32>,
    /// Inherited fd to write "READY\n" to once listening.
    #[arg(long)]
    ready_fd: Option<i32>,
    /// Attach to an existing X display instead of starting one.
    #[arg(long)]
    display: Option<String>,
    /// X server program.
    #[arg(long, default_value = "Xvfb")]
    xserver: String,
    /// Extra argument for the X server (repeatable).
    #[arg(long = "xserver-arg")]
    xserver_args: Vec<String>,
    /// Initial screen width.
    #[arg(long, default_value_t = 1920)]
    width: u32,
    /// Initial screen height.
    #[arg(long, default_value_t = 1080)]
    height: u32,
    /// Maximum screen width.
    #[arg(long, default_value_t = 4096)]
    max_width: u32,
    /// Maximum screen height.
    #[arg(long, default_value_t = 2160)]
    max_height: u32,
    /// DPI reported by the X server.
    #[arg(long, default_value_t = 96)]
    dpi: u32,
    /// Command that starts the desktop ("none" to start nothing).
    #[arg(long, default_value = "/etc/lynxrdp/startwm.sh")]
    startwm: String,
    /// Frame rate cap.
    #[arg(long, default_value_t = 60)]
    max_fps: u32,
    /// Frames in flight before waiting for acknowledgements.
    #[arg(long, default_value_t = 2)]
    max_in_flight: u32,
    /// Exit when the client disconnects.
    #[arg(long)]
    exit_on_disconnect: bool,
    /// Exit after this many seconds without a client (0 = never).
    #[arg(long, default_value_t = 0)]
    idle_timeout: u64,
    /// Accept clients from any local uid (testing only).
    #[arg(long)]
    insecure_skip_peer_check: bool,
    /// Session id reported to clients.
    #[arg(long, default_value_t = 0)]
    session_id: u64,
    /// User name reported to clients.
    #[arg(long)]
    username: Option<String>,
    /// Private directory for authority files and sockets.
    #[arg(long)]
    runtime_dir: Option<PathBuf>,
    /// Print the display of the started X server to stdout.
    #[arg(long)]
    print_display: bool,
}

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: installing a minimal async-signal-safe handler.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as *const () as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
        // We do our own child reaping; never let a closed socket kill us.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(e) => {
            log::error!("{e:#}");
            eprintln!("lynxrdp-session: {e:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    let args = Args::parse();
    install_signal_handlers();

    if args.listen.is_none() && args.control_fd.is_none() {
        bail!("nothing to listen on: pass --listen ADDR:PORT or --control-fd N");
    }
    let username = args.username.clone().unwrap_or_else(current_username);
    let runtime_dir = args.runtime_dir.clone().unwrap_or_else(default_runtime_dir);

    // 1. X server.
    let (xserver, display_name) = match &args.display {
        Some(d) => (None, d.clone()),
        None => {
            let cfg = XServerConfig {
                program: args.xserver.clone(),
                extra_args: args.xserver_args.clone(),
                max_width: args.max_width.max(args.width),
                max_height: args.max_height.max(args.height),
                dpi: args.dpi,
                runtime_dir: runtime_dir.clone(),
            };
            let xs = XServer::spawn(&cfg)?;
            std::env::set_var("XAUTHORITY", xs.xauth_path());
            let d = xs.display();
            (Some(xs), d)
        }
    };
    std::env::set_var("DISPLAY", &display_name);
    if args.print_display {
        println!("{display_name}");
    }
    let display = Arc::new(XDisplay::connect(&display_name, Duration::from_secs(10))?);

    // Size the screen before starting the desktop so it lays out correctly.
    if xserver.is_some() {
        if let Err(e) =
            lynxrdp_server::x11::resize::resize_screen(&display, args.width, args.height)
        {
            log::warn!("initial resize failed: {e:#}");
        }
    }

    let (tx, rx) = crossbeam_channel::unbounded::<CoreEvent>();

    // 2. Desktop session.
    let mut desktop = if args.startwm != "none" {
        let xauth = xserver
            .as_ref()
            .map(|x| x.xauth_path().to_path_buf())
            .unwrap_or_else(|| {
                std::env::var_os("XAUTHORITY")
                    .map(PathBuf::from)
                    .unwrap_or_default()
            });
        let mut env = HashMap::new();
        env.insert(
            "LYNXRDP_SESSION_ID".to_string(),
            args.session_id.to_string(),
        );
        let d = Desktop::spawn(&args.startwm, &display_name, &xauth, &env)?;
        Some(d)
    } else {
        None
    };

    // 3. Listeners.
    let require_uid = if args.insecure_skip_peer_check {
        None
    } else {
        Some(lynxrdp_server::peer::own_uid())
    };
    let mut listener_threads = Vec::new();
    if let Some(addr) = &args.listen {
        let l = TcpListener::bind(addr).with_context(|| format!("binding {addr}"))?;
        let local = l.local_addr()?;
        if !local.ip().is_loopback() {
            bail!("refusing to listen on non-loopback address {local}");
        }
        log::info!(
            "listening on {local} (peer uid check: {})",
            require_uid.is_some()
        );
        listener_threads.push(spawn_tcp_listener(l, tx.clone(), require_uid));
    }
    if let Some(fd) = args.control_fd {
        // SAFETY: the fd was handed to us by lynxrdpd for exactly this purpose.
        let l = unsafe { UnixListener::from_raw_fd(fd) };
        listener_threads.push(spawn_control_listener(
            l,
            tx.clone(),
            lynxrdp_server::peer::own_uid(),
        ));
        log::info!("accepting handoffs on control fd {fd}");
    }
    if let Some(fd) = args.ready_fd {
        // SAFETY: fd provided by the parent for the readiness signal.
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        use std::io::Write;
        let _ = f.write_all(b"READY\n");
    }

    // 4. Core.
    let opts = SessionOptions {
        max_fps: args.max_fps.clamp(1, 240),
        max_in_flight: args.max_in_flight.clamp(1, 8),
        max_width: args.max_width.max(args.width),
        max_height: args.max_height.max(args.height),
        default_width: args.width,
        default_height: args.height,
        username,
        session_id: args.session_id,
        exit_on_disconnect: args.exit_on_disconnect,
        idle_timeout: if args.idle_timeout > 0 {
            Some(Duration::from_secs(args.idle_timeout))
        } else {
            None
        },
        require_uid,
    };
    let mut core = Core::new(display.clone(), opts, tx.clone(), rx)?;
    let _x_thread = Core::spawn_x_event_thread(display.clone(), tx.clone());

    // 5. Watchers: desktop exit and signals.
    if let Some(d) = desktop.as_mut() {
        let pid = d.pid();
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("desktop-wait".into())
            .spawn(move || {
                // Poll rather than wait() so the Desktop object keeps ownership.
                loop {
                    let mut status = 0;
                    // SAFETY: waitpid on our child with WNOHANG.
                    let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
                    if r == pid as i32 {
                        let _ = tx.send(CoreEvent::DesktopExited(format!("status {status}")));
                        break;
                    }
                    if r < 0 {
                        let _ = tx.send(CoreEvent::DesktopExited("wait failed".into()));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            })
            .context("spawn desktop waiter")?;
    }
    {
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("signal-watch".into())
            .spawn(move || loop {
                if SHUTDOWN.load(Ordering::SeqCst) {
                    let _ = tx.send(CoreEvent::Shutdown("signal".into()));
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            })
            .context("spawn signal watcher")?;
    }

    let exit = core.run();
    log::info!("session ending: {exit:?}");
    if let Some(mut d) = desktop.take() {
        if !matches!(exit, Exit::DesktopExited(_)) {
            d.shutdown();
        }
    }
    drop(core);
    if let Some(mut xs) = xserver {
        xs.shutdown();
    }
    Ok(match exit {
        Exit::XError(_) => 2,
        _ => 0,
    })
}

fn current_username() -> String {
    if let Ok(u) = std::env::var("USER") {
        if !u.is_empty() {
            return u;
        }
    }
    let uid = lynxrdp_server::peer::own_uid();
    // SAFETY: getpwuid_r with our own buffers.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0u8; 4096];
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            uid,
            &mut pw,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        );
        if rc == 0 && !result.is_null() {
            return std::ffi::CStr::from_ptr(pw.pw_name)
                .to_string_lossy()
                .into_owned();
        }
    }
    format!("uid{uid}")
}
