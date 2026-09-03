# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

LynxRDP is a from-scratch remote desktop stack in Rust: a Linux server that
serves X11 sessions over loopback only, and a GUI client for Windows, macOS and
Linux that reaches it through an SSH tunnel. There is no RDP or VNC code here —
the wire protocol is our own.

Read [ARCHITECTURE.md](ARCHITECTURE.md) for how the pieces fit together and
[SECURITY.md](SECURITY.md) for the threat model before changing anything in
`crates/server` or the transfer/clipboard paths. This file covers what those
two do not: commands, invariants, and the traps that have already cost a CI
cycle.

## Commands

The CI workflow (`.github/workflows/ci.yml`) is the source of truth. These are
the same steps:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo test --workspace                                   # 234 tests
```

Narrower runs while iterating:

```bash
cargo test -p lynxrdp-proto                    # one crate
cargo test -p lynxrdp-server --lib             # unit tests only, no e2e
cargo test -p lynxrdp-server --test e2e -- --test-threads=2
cargo test -p lynxrdp-server --test daemon -- --test-threads=1
cargo test --workspace codec::                 # by module path
cargo test -p lynxrdp-proto codec::tests::noisy_tile_roundtrip -- --exact --nocapture
```

The thread limits are not decoration: `e2e` starts real `Xvfb` displays and
`daemon` binds real sockets and spawns processes. Running them wide is flaky.

The Python monitor viewer is a separate suite (75 tests):

```bash
cd tools/lynxrdp-monitor
pip install -r requirements.txt pytest
QT_QPA_PLATFORM=offscreen python -m pytest tests/ -q
```

`QT_QPA_PLATFORM=offscreen` is required — the GUI tests drive a real Qt window
and there is no display in CI.

### Running the client

```bash
cargo run -p lynxrdp-client --bin lynxrdp                  # connection manager
cargo run -p lynxrdp-client --bin lynxrdp -- user@host     # straight to a session
LYNXRDP_CONFIG_DIR=/tmp/cfg cargo run -p lynxrdp-client --bin lynxrdp
```

`LYNXRDP_CONFIG_DIR` overrides where saved connections live and is the way to
exercise the launcher without touching your real `connections.toml`.

To drive the GUI headlessly (how the launcher was verified):

```bash
Xvfb :77 -screen 0 1024x700x24 &
DISPLAY=:77 cargo run -p lynxrdp-client --bin lynxrdp &
DISPLAY=:77 xdotool search --name "LynxRDP" ...
DISPLAY=:77 scrot -o /tmp/shot.png     # then read the PNG back
```

### Packaging

Every script is runnable locally; none needs the platform it targets except
where noted.

```bash
packaging/package-server.sh amd64                    # .deb + .rpm (needs nfpm)
packaging/package-client.sh x86_64-unknown-linux-gnu linux-x86_64 lynxrdp
packaging/make-setup-exe.sh path/to/lynxrdp.exe dist # Windows installer; works on Linux
packaging/make-app-bundle.sh path/to/lynxrdp stage   # LynxRDP.app
packaging/make-dmg.sh stage/LynxRDP.app dist macos-aarch64   # macOS only (hdiutil)
assets/generate-icons.sh                             # only when the SVG changes
```

`make-setup-exe.sh` needs `makensis`: `apt install nsis` on Linux builds Windows
installers fine. Generated icons are committed, so a normal build rasterises
nothing.

## Architecture

`ARCHITECTURE.md` has the detail. The parts worth knowing before you edit:

**Three processes, decreasing privilege.** `lynxrdpd` (root) listens on
loopback, identifies the connecting user (`SO_PEERCRED` for Unix sockets,
a `/proc/net/tcp` lookup for loopback TCP — `peer.rs`), opens a PAM *session*
(authentication already happened over SSH), drops privileges,
and hands the connected socket to a per-user `lynxrdp-session` over
`SCM_RIGHTS`. The session never runs as root and the daemon never touches pixel
data. `crates/server/src/handoff.rs` and `daemon/supervisor.rs` are where that
seam lives.

**The client is one binary with two entry points.** No arguments opens the egui
connection manager (`launcher.rs`); a destination opens a session
(`app.rs` + `connection.rs`). The launcher starts sessions by **re-invoking
itself as a child process** (`launch.rs`). That is forced — a process may hold
only one winit event loop, and eframe already holds it — so do not try to open
a session in-process. The upside is that a profile's arguments are exactly what
a user could type, so the two entry points cannot drift.

**`crates/proto` is shared by both sides** and has no I/O. Anything that changes
the wire format (`message.rs`, `frame.rs`, `codec.rs`, `transfer.rs`) changes
both ends at once; there is no version negotiation beyond the handshake, so
server and client must be built from the same commit.

**The headless client library is the test harness.** `lynxrdp_client::connection`
is a full protocol client with no window, which is what `tests/e2e.rs` drives
against a real session on Xvfb. New protocol features should be reachable from
it, or they cannot be tested end to end.

## Invariants

These are load-bearing. Breaking one is a security or compatibility regression,
not a style question.

- **Loopback only.** `lynxrdpd` refuses a non-loopback bind at two layers
  (`config.rs` validation and a check in `lynxrdpd.rs` after binding). Both
  stay. The server is reachable only through an SSH tunnel by design; there is
  no "listen on 0.0.0.0" option to add.
- **No passwords or passphrases on disk.** Saved connections hold host, user,
  port, identity *path*, and display options — never a secret. SSH owns
  authentication. `profiles.rs` has no field for one; do not add one.
- **Monitoring reports are obfuscated, not encrypted.** The ChaCha20-Poly1305
  key is compiled in *and* printed in `reporting/seal.rs`. It stops a `tcpdump`
  from reading as an inventory of hostnames; it stops nothing else. Say so
  plainly in any docs you touch. The wire format carries a version byte so a
  real per-deployment key can be added later without a flag day.
- **Rust and Python are pinned to each other.** The report format is asserted by
  a known-answer test on both sides (`reporting/seal.rs` and
  `tools/lynxrdp-monitor/tests/test_crypto.py`). Change one, change both, or the
  suites diverge silently.
- **Server packages are built against the RHEL 9 glibc** inside an AlmaLinux 9
  container, because glibc is backward but not forward compatible — binaries
  linked against the runner's newer glibc will not start on RHEL 9.
  `packaging/check-glibc-floor.sh` enforces it in CI.
- **Pure Rust, no C library dependencies.** x11rb, not xlib; PAM is `dlopen`ed
  at runtime (`daemon/pam.rs`) so one binary works with or without PAM present.
  Keep new dependencies in that spirit.

## Platform traps

Real failures from this repo, each of which passed on Linux first:

- **NSIS wants native Windows paths.** Under Git Bash, MSYS rewrites
  `/d/a/...` to `D:/a/...` on the way to `makensis.exe`, and NSIS reads the
  forward slashes literally — "no files found" on a file that is plainly
  there. `make-setup-exe.sh` runs paths through `cygpath -w` for this reason.
  Linux `makensis` accepts forward slashes, so a local test will not catch it.
- **NSIS is not on the GitHub Windows runners.** CI installs it with
  `choco install nsis`; do not assume the image has it.
- **`[ -x "$f" ] && VAR=...` exits a `set -e` script** when the test simply
  fails. Use `if`. Every packaging script is shellcheck-clean; keep it that way.
- **The Windows client is built for the GUI subsystem**
  (`#![windows_subsystem = "windows"]`), so it does not flash a console from
  Explorer. `console.rs` reattaches to the parent terminal to keep the command
  line working — if you add early output, make sure it still lands there.
- Clipboard file lists have no cross-platform crate: X11 `text/uri-list`,
  Windows `CF_HDROP`, macOS `NSPasteboard`, three implementations in
  `fileclip.rs` behind one interface. A change to one usually needs all three.

## Releases

Tag pushes are typically rejected in agent sessions (HTTP 403 — credentials are
scoped to the working branch). The release workflow therefore takes a
`workflow_dispatch` input and **creates the tag itself**:

```
Actions → Release → Run workflow → tag: v0.1.0-rc.4
```

A hyphen in the tag marks it a prerelease automatically. This is how every
release so far has been cut. If a `git push origin <tag>` returns 403, that is
expected — use the dispatch, do not try to work around the credential scope.

CI builds the Windows `setup.exe` and macOS `.dmg` on **every** run, not only
at release time, so a broken installer script fails the pull request that broke
it rather than a release.

## Conventions

Comments explain *why*, in prose, and are worth reading before matching them —
this codebase leans harder on that than most. A comment that restates the code
is noise; a comment recording the constraint that forced the code (a
platform quirk, a protocol rule, a rejected simpler approach) is the norm here.
The same applies to commit messages: they explain the reasoning, not just the
change.

`main` uses merge commits, not squashes.
