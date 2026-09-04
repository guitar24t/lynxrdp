//! `lynxrdp`: the desktop client and its command line.
//!
//! Started with no arguments it opens the connection manager; with a
//! destination it opens a session directly. Both are the same binary, and the
//! launcher starts sessions by re-invoking it.

// Built for the Windows GUI subsystem so that opening the launcher from
// Explorer does not flash up a console. The command line keeps working:
// `console::attach_to_parent` rejoins the terminal we were typed into. On
// every other platform this attribute does nothing.
#![windows_subsystem = "windows"]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lynxrdp_client::app::{App, AppOptions, Session};
use lynxrdp_client::connection::{Client, ConnectOptions};
use lynxrdp_client::profiles::MAX_SCALE;
use lynxrdp_client::tunnel::{parse_destination, Endpoint, RemoteTarget, Tunnel, TunnelConfig};

// Part of the binary rather than the library: it is an entry point, reached
// only through this file's argument handling, and nothing that links the
// client as a library has any use for it.
mod askpass;

/// Connect to a LynxRDP session over SSH.
#[derive(Parser, Debug)]
#[command(name = "lynxrdp", version, about, long_about = None)]
struct Args {
    /// Transfer files instead of opening a desktop window.
    #[command(subcommand)]
    command: Option<Command>,
    /// SSH destination: [user@]host[:port] or a ~/.ssh/config alias.
    destination: Option<String>,
    /// SSH port (overrides a port given in the destination).
    #[arg(global = true, short = 'p', long)]
    port: Option<u16>,
    /// SSH identity file.
    #[arg(global = true, short = 'i', long)]
    identity: Option<PathBuf>,
    /// Extra ssh -o option (repeatable), e.g. -o ProxyJump=bastion
    #[arg(global = true, short = 'o', long = "ssh-option")]
    ssh_options: Vec<String>,
    /// Path of the ssh executable.
    #[arg(global = true, long, default_value = "ssh")]
    ssh: String,
    /// LynxRDP port on the remote host's loopback interface.
    #[arg(global = true, long, default_value_t = lynxrdp_proto::DEFAULT_PORT)]
    remote_port: u16,
    /// Forward to a Unix socket on the remote host instead of a TCP port.
    #[arg(long)]
    remote_socket: Option<String>,
    /// Bind the tunnel's local end to this loopback TCP port instead of a
    /// private Unix socket. Every process on this machine can then reach it.
    #[arg(long, default_value_t = 0)]
    local_port: u16,
    /// Connect directly to an already tunnelled loopback address and skip
    /// starting ssh (e.g. 127.0.0.1:3390). Only loopback addresses are accepted.
    #[arg(long, conflicts_with = "destination")]
    connect: Option<SocketAddr>,
    /// Initial remote screen size, e.g. 1920x1080 (default: server default).
    #[arg(long, value_parser = parse_size)]
    size: Option<(u16, u16)>,
    /// Start in fullscreen (toggle with Ctrl+Alt+Enter).
    #[arg(short = 'f', long)]
    fullscreen: bool,
    /// Magnify the remote screen by a whole number, 1 to 4
    /// (default: match the display, so a 2x screen gets 2).
    #[arg(long, value_parser = parse_scale)]
    scale: Option<u8>,
    /// Keep the remote screen size fixed instead of following the window.
    #[arg(long)]
    no_dynamic_resize: bool,
    /// Do not synchronise the clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Seconds to wait for the tunnel (ssh may prompt for credentials).
    #[arg(long, default_value_t = 120)]
    tunnel_timeout: u64,
}

/// File transfer subcommands. Each opens the same SSH tunnel the desktop
/// client uses and talks to the user's existing session.
#[derive(Subcommand, Debug)]
enum Command {
    /// Copy local files into the session.
    Send {
        /// SSH destination: [user@]host[:port].
        destination: String,
        /// Files to upload.
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Destination directory relative to the session's upload directory.
        #[arg(long)]
        into: Option<String>,
    },
    /// Copy a file out of the session.
    Get {
        /// SSH destination: [user@]host[:port].
        destination: String,
        /// Path to read inside the session.
        remote: String,
        /// Where to write it locally (default: the file's name here).
        local: Option<PathBuf>,
    },
}

/// Only whole factors, and only small ones.
///
/// The magnification is nearest-neighbour, which is what keeps the promise
/// that a remote desktop is never resampled and text stays exactly as the
/// server drew it; that only holds at an integer factor. The upper bound is
/// the profile's, so a connection saved in the launcher and a connection
/// typed here cannot disagree about what is allowed.
fn parse_scale(s: &str) -> Result<u8, String> {
    let n: u8 = s
        .parse()
        .map_err(|_| "expected a whole number".to_string())?;
    if !(1..=MAX_SCALE).contains(&n) {
        return Err(format!("the scale must be between 1 and {MAX_SCALE}"));
    }
    Ok(n)
}

fn parse_size(s: &str) -> Result<(u16, u16), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| "expected WIDTHxHEIGHT".to_string())?;
    let w: u16 = w.parse().map_err(|_| "bad width".to_string())?;
    let h: u16 = h.parse().map_err(|_| "bad height".to_string())?;
    if w < 64 || h < 64 {
        return Err("size must be at least 64x64".into());
    }
    Ok((w, h))
}

fn main() {
    // Before everything, including the standard handles. ssh runs this same
    // binary as its askpass helper and reads the answer from our stdout, so
    // `attach_to_parent` -- which points stdout at the terminal the launcher
    // was started from -- would send the answer somewhere ssh is not looking.
    // The prompt also arrives as a bare positional argument that clap would
    // read as a destination.
    if let Some(code) = askpass::run_if_helper() {
        std::process::exit(code);
    }
    // Before the logger: it writes to stderr, which does not exist yet.
    lynxrdp_client::console::attach_to_parent();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    if let Err(e) = run() {
        eprintln!("lynxrdp: {e:#}");
        std::process::exit(1);
    }
}

/// The SSH invocation a destination on this command line asks for.
fn tunnel_config(args: &Args, destination: &str) -> Result<TunnelConfig> {
    let (destination, port_in_dest) = parse_destination(destination)?;
    Ok(TunnelConfig {
        destination,
        ssh_port: args.port.or(port_in_dest),
        identity: args.identity.clone(),
        options: args.ssh_options.clone(),
        remote: match &args.remote_socket {
            Some(p) => RemoteTarget::Socket(p.clone()),
            None => RemoteTarget::Port(args.remote_port),
        },
        local_port: args.local_port,
        ssh_program: args.ssh.clone(),
        extra_args: Vec::new(),
        // Empty unless the connection manager started us, so a session typed
        // at a terminal keeps prompting there.
        env: askpass::ssh_env(),
    })
}

fn run() -> Result<()> {
    let args = Args::parse();
    if let Some(command) = &args.command {
        return run_transfer_command(&args, command);
    }
    // The endpoint, not a connection: the session window keeps it and dials it
    // again if the link drops, so the way back is the same way it came. The
    // first connection goes through it too, which is what stops the two paths
    // from drifting -- the reconnect path is exercised by every start.
    let mut endpoint = match (&args.connect, &args.destination) {
        (Some(addr), _) => Endpoint::direct(*addr)?,
        (None, Some(dest)) => Endpoint::ssh(
            tunnel_config(&args, dest)?,
            Duration::from_secs(args.tunnel_timeout),
        ),
        (None, None) => {
            // No arguments is not a usage error any more: this is a desktop
            // application, so show the connection manager. The CLI is still
            // there for anyone who wants it, and for the sessions this
            // launches, which are this same binary with a destination.
            // Marked before the window opens, because the sessions this
            // starts inherit the environment and that is how each of them
            // knows to answer ssh in a window rather than on a terminal it
            // does not have.
            askpass::mark_launcher();
            let path = lynxrdp_client::launcher::default_path()?;
            return lynxrdp_client::launcher::run(path);
        }
    };

    let (waker, slot) = lynxrdp_client::app::make_waker();
    let opts = ConnectOptions {
        size: args.size,
        ..Default::default()
    };
    // For an SSH endpoint this starts ssh and hands over the connection its
    // readiness check already made: dialling a second one would be a second
    // client as far as the daemon is concerned, and it would replace the
    // session this one just attached to.
    let stream = endpoint.connect()?;
    let client = Client::from_stream(stream.into_tcp(), &opts, Some(waker))
        .context("connecting to the LynxRDP server")?;
    log::info!(
        "connected to {} as {} (session {}), screen {}x{}",
        client.info().server_name,
        client.info().username,
        client.info().session_id,
        client.size().0,
        client.size().1
    );
    let app_opts = AppOptions {
        fullscreen: args.fullscreen,
        title: "LynxRDP".to_string(),
        dynamic_resize: !args.no_dynamic_resize,
        clipboard: !args.no_clipboard,
        scale: args.scale,
    };
    let session = Session {
        endpoint: Some(endpoint),
        connect: opts,
        waker: slot,
    };
    let reason = App::run(client, app_opts, session)?;
    if let Some(r) = reason {
        log::info!("session closed: {r}");
        if r.starts_with("rejected") || r.starts_with("protocol error") {
            bail!("{r}");
        }
    }
    Ok(())
}

/// Open a tunnel and a connection to the destination, without a window.
fn connect_headless(args: &Args, destination: &str) -> Result<(Client, Option<Tunnel>)> {
    let cfg = tunnel_config(args, destination)?;
    let mut tunnel = Tunnel::open(&cfg, Duration::from_secs(args.tunnel_timeout))?;
    let opts = ConnectOptions::default();
    // Reuse the connection the readiness check already made. Dialling again
    // would be a second client as far as the daemon is concerned, which
    // replaces the session the first one just attached to -- and on a Unix
    // local end there is no address to dial in the first place.
    let stream = tunnel
        .take_stream()
        .context("the tunnel came up without a connection")?;
    let client = Client::from_stream(stream.into_tcp(), &opts, None)
        .context("connecting to the LynxRDP server")?;
    Ok((client, Some(tunnel)))
}

/// How long one file transfer may take before giving up.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(3600);

fn run_transfer_command(args: &Args, command: &Command) -> Result<()> {
    match command {
        Command::Send {
            destination,
            files,
            into,
        } => {
            for f in files {
                if !f.is_file() {
                    bail!("{} is not a regular file", f.display());
                }
            }
            let (mut client, _tunnel) = connect_headless(args, destination)?;
            for local in files {
                let name = local
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .ok_or_else(|| anyhow::anyhow!("{} has no file name", local.display()))?;
                let dest = match into {
                    Some(dir) => format!("{}/{name}", dir.trim_end_matches('/')),
                    None => name.clone(),
                };
                let size = std::fs::metadata(local)?.len();
                let id = client.send_file(local, &dest)?;
                client.run_transfer(id, TRANSFER_TIMEOUT)?;
                println!("sent {} ({size} bytes) as {dest}", local.display());
            }
            client.disconnect("transfer complete");
            Ok(())
        }
        Command::Get {
            destination,
            remote,
            local,
        } => {
            let dest = match local {
                Some(p) if p.is_dir() => {
                    let name = remote.rsplit('/').next().unwrap_or("download");
                    p.join(name)
                }
                Some(p) => p.clone(),
                None => PathBuf::from(remote.rsplit('/').next().unwrap_or("download")),
            };
            let (mut client, _tunnel) = connect_headless(args, destination)?;
            let id = client.request_file(remote, dest.clone())?;
            client.run_transfer(id, TRANSFER_TIMEOUT)?;
            let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            println!("received {remote} ({size} bytes) into {}", dest.display());
            client.disconnect("transfer complete");
            Ok(())
        }
    }
}
