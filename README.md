# LynxRDP

A small, fast, security-first remote desktop system for Linux servers with
clients for Windows, macOS and Linux.

LynxRDP gives every user their own headless desktop session on the server,
like `xrdp`/FreeRDP do, but with a much smaller and more predictable code
base and one non-negotiable rule: **the server only ever listens on
loopback**. Clients reach it through an SSH port forward, so the only thing
exposed to the network is `sshd`, and the only credentials that exist are
your SSH credentials.

```
 workstation                                  server (Linux)
 ┌──────────────┐   ssh -L 3390:127.0.0.1:3390   ┌─────────────────────────┐
 │ lynxrdp  ────┼──────── encrypted ────────────▶│ sshd ─▶ 127.0.0.1:3390   │
 │ (client GUI) │                                │          │ lynxrdpd      │
 └──────────────┘                                │          ▼ (who is it?)  │
                                                 │   lynxrdp-session (user)│
                                                 │   Xvfb + your desktop   │
                                                 └─────────────────────────┘
```

## Features

* **No passwords in the protocol.** The daemon identifies the local user who
  owns the socket that connected to it (the sshd process that carries the
  port forward runs as that user) and starts or resumes *that* user's session.
* **Persistent sessions.** Disconnect and reconnect from anywhere; your
  desktop keeps running. Sessions also survive daemon restarts and package
  upgrades.
* **Low latency by design.** Damage tracking, shared-memory capture, tile
  diffing, local cursor rendering, ack-based flow control that never lets
  stale frames queue up, `TCP_NODELAY` everywhere.
* **A codec built for text, not video.** Lossless, so glyph edges stay
  sharp and the client's screen is bit-identical to the server's. Scrolling
  is sent as a copy rectangle rather than re-encoded (measured 40x smaller
  for line-by-line scrolling), tiles of flat colour become a palette with
  1–8 bit indices, and payloads are compressed with whichever of LZ4 or
  Zstd is smaller. An idle screen costs zero bytes.
* **Dynamic resolution.** Resize the window and the remote screen follows;
  no scaling, no blurry text.
* **Clipboard sync** in both directions: text, images (as lossless PNG,
  fetched only when the other side asks, so copying an image you never
  paste costs nothing), and files copied in the session — those paste into
  Explorer, Finder and Linux file managers alike.
* **File transfer.** Drop files or folders on the window to upload them,
  or use `lynxrdp send host ./file` and `lynxrdp get host ~/file`. Both
  reuse the same SSH tunnel; transfers are chunked and windowed so a large
  file never stalls the interactive screen stream.
* **A desktop application, not just a command.** The client opens a
  connection manager with saved connections, and installs like any other
  program: a Start Menu entry on Windows, `LynxRDP.app` on macOS, an
  applications-menu entry on Linux. Every command line option is still there.
* **Pure Rust, tiny dependency surface.** No C libraries are linked at build
  time. The X server (Xvfb) and `libpam` are used at runtime.
* **Runs without root too.** `lynxrdp-session --listen 127.0.0.1:3390` serves
  your own session with the same security property, no daemon needed.
* **Optional fleet view.** Servers can report their hostname and address to a
  monitoring server on an interval, and `tools/lynxrdp-monitor` shows who is
  up and copies an address to the clipboard. Off by default, outbound only.

Supported server distributions: RHEL 9 (and derivatives), Ubuntu 24.04 and
26.04, and in practice any Linux with systemd, Xvfb and PAM.

## Installing the server

Download the `.deb` or `.rpm` for your architecture from the releases page,
or build it yourself (see below).

```sh
# Debian / Ubuntu
sudo apt install ./lynxrdp-server_*_amd64.deb
# RHEL / Fedora
sudo dnf install ./lynxrdp-server-*.x86_64.rpm
```

The package installs `lynxrdpd`, `lynxrdp-session`, a systemd unit (enabled
and started), `/etc/lynxrdp/lynxrdp.toml`, `/etc/lynxrdp/startwm.sh` and a
PAM service file `/etc/pam.d/lynxrdp`.

You need a desktop environment on the server. XFCE is a good, light choice:

```sh
sudo apt install xfce4 xfce4-session dbus-x11        # Ubuntu
sudo dnf groupinstall "Xfce"                         # RHEL 9 (EPEL)
```

`startwm.sh` picks the first desktop it finds (XFCE, KDE Plasma, MATE,
Cinnamon, GNOME, LXQt, ...). Users can override it with `~/.lynxrdp/session`
or `~/.xsession`.

Check the daemon:

```sh
systemctl status lynxrdpd
sudo lynxrdpd --check          # validates /etc/lynxrdp/lynxrdp.toml
sudo lynxrdpd --dump-config    # prints the effective configuration
```

## Installing the client

The client is a desktop application. Every platform has an installer on the
releases page:

| Platform | Download | Installs |
| --- | --- | --- |
| Windows 10/11 (x86_64) | `lynxrdp-<version>-windows-x86_64-setup.exe` | Program Files, Start Menu, optional desktop shortcut |
| macOS (Apple Silicon) | `lynxrdp-<version>-macos-aarch64.dmg` | drag `LynxRDP.app` to Applications |
| Debian / Ubuntu | `lynxrdp-client_<version>_<arch>.deb` | `/usr/bin/lynxrdp` plus an applications-menu entry |
| RHEL / Fedora | `lynxrdp-client-<version>.<arch>.rpm` | the same |

Plain archives are published too, for anyone who wants only the binary. The
macOS archive contains `LynxRDP.app`, with a `lynxrdp` symlink beside it for
command line use.

Intel Macs have no prebuilt download; `cargo build --release -p lynxrdp-client`
still targets them.

### The installers are not signed

There is no Apple Developer certificate and no Windows code-signing
certificate behind this project, so both systems will object the first time:

* **Windows** shows *"Windows protected your PC"*. Click **More info**, then
  **Run anyway**. The UAC prompt afterwards says *Unknown publisher*, which
  is the same fact stated twice.
* **macOS** refuses the first launch outright. Right-click (or Control-click)
  `LynxRDP.app` and choose **Open**, then **Open** again in the dialog. That
  registers your consent; ordinary double-clicks work from then on. Under
  macOS 15 the button may instead be under *System Settings → Privacy &
  Security → Open Anyway*.

Every release also publishes `SHA256SUMS`, which is the check that actually
tells you the download is intact.

### You also need an OpenSSH client

* **Windows 10/11**: ships with OpenSSH (`ssh.exe`). Enable it under
  *Settings → Apps → Optional features* if it is missing.
* **macOS**: included.
* **Linux**: `openssh-client` / `openssh-clients`, plus `libxkbcommon-x11`
  (the `.deb`/`.rpm` client packages declare these).

## Connecting

### From the connection manager

Open LynxRDP from the Start Menu, Applications or your desktop's menu — or
run `lynxrdp` with no arguments — and you get the connection manager: a list
of saved connections with **Connect**, **New**, **Edit** and **Delete**.
Double-clicking a connection opens it.

A saved connection holds the host, user, SSH port, identity file, extra
`ssh -o` options, screen size, and the fullscreen, dynamic-resize and
clipboard switches. **It never holds a password or a passphrase**: SSH
already owns authentication, and there is nothing here worth a second,
weaker copy of it.

They live in one editable TOML file:

| Platform | Path |
| --- | --- |
| Linux | `~/.config/lynxrdp/connections.toml` (or `$XDG_CONFIG_HOME`) |
| macOS | `~/Library/Application Support/LynxRDP/connections.toml` |
| Windows | `%APPDATA%\LynxRDP\connections.toml` |

Each session opens as its own process, so several can run at once and one
crashing cannot take the manager with it. Closing the manager leaves running
sessions alone.

### From the command line

```sh
lynxrdp user@server.example.org
```

That is all. The client runs `ssh -N -L <local>:127.0.0.1:3390 user@server`,
waits for the tunnel, connects through it and opens a window. Your SSH
keys, agent, `~/.ssh/config` aliases, jump hosts and multi-factor prompts
work exactly as for a shell login.

On Windows the client is a GUI application, so opening it from Explorer never
flashes up a console. Typed at a prompt it still prints normally — it
reattaches to the terminal it was started from — but `cmd.exe` returns to the
prompt without waiting for it to finish, which is inherent to a windowed
program rather than something the client chooses.

Useful options:

```
lynxrdp -p 2222 user@host           # SSH port
lynxrdp -i ~/.ssh/work user@host    # identity file
lynxrdp -o ProxyJump=bastion host   # any ssh -o option (repeatable)
lynxrdp --size 2560x1440 host       # initial remote screen size
lynxrdp -f host                     # fullscreen (toggle: Ctrl+Alt+Enter)
lynxrdp --remote-socket /run/lynxrdp/lynxrdp.sock host   # if the server
                                    # is configured with listen.unix_socket
lynxrdp --connect 127.0.0.1:3390    # use a tunnel you started yourself
```

Closing the window disconnects but leaves the session running. Reconnect
later to pick up where you left off. Connecting again from another device
takes the session over.

## Running without the daemon ("user mode")

If you cannot or do not want to install a system service, run your own
session process on the server:

```sh
lynxrdp-session --listen 127.0.0.1:3390 --startwm /etc/lynxrdp/startwm.sh
```

It refuses connections from sockets not owned by your uid, which is the same
guarantee the daemon provides, so `lynxrdp you@server` works unchanged.

## Configuration

`/etc/lynxrdp/lynxrdp.toml` is documented inline. Highlights:

| Key | Default | Meaning |
| --- | --- | --- |
| `listen.address` | `127.0.0.1` | Must be loopback; anything else is refused at startup. |
| `access.min_uid` | `1000` | System accounts and root cannot open sessions. |
| `access.allow_users`, `allow_groups` | empty | Restrict who may connect. |
| `session.default_width/height` | `1920x1080` | Size when the client does not ask for one. |
| `session.max_width/height` | `4096x2160` | Upper bound (Xvfb virtual screen). |
| `session.max_fps`, `max_in_flight` | `60`, `2` | Latency/smoothness knobs. |
| `session.idle_timeout_secs` | `0` | End sessions nobody is connected to. |

Session logs are written to `/var/log/lynxrdp/<user>.log`.

## Building from source

Requirements: Rust 1.80+ (stable). For the Linux end-to-end tests: `Xvfb`,
`xterm`, `xdotool`, `xclip`, `x11-utils`, `x11-xserver-utils`.

```sh
cargo build --release --workspace
cargo test --workspace                 # unit tests + Xvfb end-to-end tests
```

Packages (needs [nfpm](https://nfpm.goreleaser.com/)):

```sh
packaging/package-server.sh amd64      # dist/*.deb, dist/*.rpm
packaging/package-client.sh x86_64-unknown-linux-gnu linux-x86_64 lynxrdp
```

Installers:

```sh
# Windows, needs makensis (NSIS); works on Linux too.
packaging/make-setup-exe.sh target/release/lynxrdp.exe dist

# macOS, needs a Mac.
packaging/make-app-bundle.sh target/release/lynxrdp stage
packaging/make-dmg.sh stage/LynxRDP.app dist
```

The application icon is committed in every form the packages need, so a
normal build rasterises nothing. After changing `assets/lynxrdp.svg`, run
`assets/generate-icons.sh` (needs `librsvg2-bin`, `icoutils`, `icnsutils`)
and commit what it produces.

If those packages are meant to run on RHEL 9, build the binaries against its
glibc rather than your own. glibc is backward compatible but not forward, so
a binary linked on Ubuntu 24.04 (glibc 2.39) will not start on RHEL 9 (2.34):
the loader rejects it outright with ``version `GLIBC_2.39' not found``. CI
builds the server inside AlmaLinux 9 for this reason, and

```sh
packaging/check-glibc-floor.sh target/release/lynxrdpd
```

fails if a binary needs anything newer than the floor.

## Watching a fleet

Servers can send a heartbeat to a monitoring server so you can see which
machines are up and what address to reach them on:

```toml
# /etc/lynxrdp/lynxrdp.toml
[reporting]
enabled = true
destination = "monitor.example.org:9999"
interval_secs = 60
#node_name = "desk01"          # if you name hosts differently
```

Each interval the daemon sends one small datagram with its hostname, the
address it would be reached on, the version and how many sessions are
running. It only ever sends: no port is opened, so the loopback-only rule is
unchanged.

Reports are sealed with ChaCha20-Poly1305 so a packet capture does not read
as a list of your hostnames. The key is baked into the software, so this is
obfuscation rather than confidentiality — anyone holding the binary can
recover the key. Keep reports on a management network; see
[SECURITY.md](SECURITY.md) for exactly what that does and does not buy.

[`tools/lynxrdp-monitor`](tools/lynxrdp-monitor) is a small PySide6 viewer
that listens for them, lists the hosts and copies an address to the clipboard:

```sh
cd tools/lynxrdp-monitor && pip install -r requirements.txt && ./lynxrdp-monitor
```

## Repository layout

| Path | What |
| --- | --- |
| `crates/proto` | Wire protocol, framing, tile codec, copy detection, keysyms. Shared by both sides. |
| `crates/server` | `lynxrdpd` (daemon) and `lynxrdp-session` (per-user session). Linux only. |
| `crates/client` | `lynxrdp` GUI client and the headless client library. |
| `packaging/` | nfpm configs, systemd unit, PAM files, `startwm.sh`, the NSIS installer, the macOS bundle and disk image scripts. |
| `assets/` | The application icon: one SVG and the PNG, `.ico` and `.icns` forms generated from it. |
| `tools/lynxrdp-monitor` | PySide6 viewer for the optional heartbeat reports. |
| `.github/workflows` | CI (lint, tests, multi-OS client builds, packages) and releases. |

See [ARCHITECTURE.md](ARCHITECTURE.md) for how it works and
[SECURITY.md](SECURITY.md) for the threat model.

## License

MIT.
