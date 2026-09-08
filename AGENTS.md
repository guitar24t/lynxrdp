# AGENTS.md

Guidance for coding agents working in this repository. `AGENTS.md` is the name
most tools look for; `CLAUDE.md` is a pointer to this file so that Claude Code
finds it too. Edit this file, never that one, and the two cannot drift.

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

The CI workflow (`.github/workflows/ci.yml`) is the source of truth. The first
two of these are steps of it verbatim; the third is a stand-in, not an
equivalent — CI runs `--lib --bins`, `--doc` and the three integration suites
as separate steps, and then two things a plain `cargo test` never reaches at
all: a graphical input check that lives outside cargo
(`python3 tools/check-session-ui.py`), and the two `#[ignore]`d client tests
that need a display.

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo test --workspace                                  # Linux only; see below
```

**On macOS the second and third of those do not run — they fail to build.**
Both server binaries carry `#![cfg(target_os = "linux")]` with no `main`
outside it, and `crates/server` is an unconditional workspace member, so
anything that builds every target of every crate stops at
`error[E0601]: main function not found`. What works on a Mac is the line CI's
macOS leg runs, which covers everything that is not Linux-only:

```bash
cargo test -p lynxrdp-proto -p lynxrdp-client -p lynxrdp-filecopy
```

The server has to be built and tested on Linux.

**Build on a current stable toolchain, not just whatever is installed.** Several
rustc lints that `-D warnings` turns into errors exist only on newer releases --
the float-literal fallback on `impl Into<f32>` arguments is one that reached CI
this way. CI uses `dtolnay/rust-toolchain@stable`, so anything older than that
locally is a blind spot rather than a safe floor.

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

**`LYNXRDP_REQUIRE_E2E=1` makes a missing dependency a failure instead of a
skip.** Every guard in the integration suites prints `SKIP:` and returns, and
cargo reports a test that returns as a test that *passed* -- so without this,
dropping `xvfb` from the CI apt line would leave the whole end-to-end suite
green while covering nothing. CI sets it on exactly the steps that install those
dependencies. Set it locally when you want to be sure a run is real; leave it
unset if you genuinely do not have `Xvfb` or `xclip`, and never set it for
`privdrop`, whose root check goes through the same guard — run it as an
ordinary user and the variable turns a correct skip into a failure, which
`privdrop.rs` says in its own header.

```bash
cargo test -p lynxrdp-server --test privdrop   # needs root; skips cleanly otherwise
cargo test -p lynxrdp-server --test tunnel_e2e # needs sshd; CI runs it now
```

Two dependencies are easy to miss because nothing else in the tree needs them.
`python3` is required by an ordinary unit test -- `x11/empty_drop.rs` shells out
to a fixture script for the empty-view contract, which is why the `--lib --bins`
step sets `LYNXRDP_REQUIRE_E2E` too -- and FUSE by the clipboard-file end-to-end
tests, because `lynxrdp_filecopy::Files::new` opens `/dev/fuse` and execs
`fusermount3`. Both now skip cleanly when the dependency is absent and fail
where that variable says the environment is supposed to have it. A container
with the `fuse3` package but no device node is the case the FUSE guard checks
both halves for.

**Two tests reach github.com and are `#[ignore]`d for it.** A suite that fails
on an aeroplane is a suite people learn to ignore, so neither CI nor a plain
`cargo test` runs them. They are the only check that the release listing still
has the shape the updater reads and that a published archive still installs,
which makes them worth running by hand when either end changes:

```bash
cargo test -p lynxrdp-client --lib -- --ignored --nocapture
```

That line picks up a third everywhere, `gui_paint`'s rendering benchmark, which
needs nothing and only prints timings. On Linux it picks up two more still — the
X11 file-clipboard test and the shared-window one — which need a display and
fail rather than skip without one; CI runs exactly those two under `xvfb-run`.

Widening `--ignored` across the workspace is a different matter, because two
ignored tests rewrite checked-in fixtures. Both now refuse unless told
explicitly: `LYNXRDP_WRITE_REPORT_FIXTURE` for the monitoring fixture and
`LYNXRDP_WRITE_WIRE_CORPUS` for `crates/proto/tests/corpus/messages.hex`, so a
bare `cargo test -p lynxrdp-proto -- --ignored` fails loudly rather than
quietly rewriting the one file that holds `MIN_COMPATIBLE_VERSION` to its
promise. The corpus refuses to change an existing encoding unless that floor has
risen, and refuses to create the file at all without `LYNXRDP_CREATE_WIRE_CORPUS`
-- deleting it was previously a way to launder a wire change past the check.

**`cargo build -p lynxrdp-server` on its own can fail to link with
`unable to find library -lxcb`,** on a host with `libxcb` but no `libxcb-devel`.
Building the server *with* the client (`--workspace`, or the release line the
packaging uses) resolves x11rb's features differently and links fine, which is
why CI never sees it. Build the workspace rather than chasing it.

The Python monitor viewer is a separate suite:

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

**The client is one binary with four entry points.** No arguments opens the egui
connection manager (`launcher.rs`); a destination opens a session
(`app.rs` + `connection.rs`); a `send`/`get`/`sessions`/`terminate` subcommand
runs headless; and ssh runs this same binary as its own askpass helper, checked
in `main.rs` before clap so a prompt is not mistaken for a destination. On
Windows and Linux the launcher starts sessions by **re-invoking itself as a
child process** (`launch.rs`). That is forced — a process may hold only one
winit event loop, and eframe already holds it — so do not try to open a session
in-process. The upside is that a profile's arguments are exactly what a user
could type, so the launcher and the command line cannot drift. macOS is the
exception: one application hosts the manager and every session window in a
single event loop (`app/desktop.rs`), and `Sessions::start` queues a profile
there rather than spawning anything, so a child's pid, exit status or stderr
tail is inert on that platform.

**`crates/proto` is shared by both sides** and has no I/O. Anything that changes
the wire format (`message.rs`, `frame.rs`, `codec.rs`, `transfer.rs`) changes
both ends at once, but the two ends are deliberately *not* required to ship
together: server packages are installed by administrators while clients update
themselves, so the handshake negotiates. `PROTOCOL_VERSION` is what a build
speaks and `MIN_COMPATIBLE_VERSION` the oldest peer it will still hold a session
with; the server refuses anything below that floor and otherwise answers with
`agreed_version`, the older of the two, leaving the newer side to decide with
`can_speak` (`lib.rs`). Feature bits carry everything optional, and
`frame::EXTENSION_TAG_MIN` lets a peer discard an *unrecognised* message tagged
128 or above rather than drop the link over it.

The obligation that comes with the floor is the expensive half, and the reason
to read `lib.rs` before touching a layout: while it stays where it is, no message
that existed at that version may change shape. An older peer decodes the new
bytes with the layout it was built with, so a field that moved or changed width
is not an error to it — it is a plausible wrong value it will act on, in a
deployed session rather than in CI. `crates/proto/tests/wire_corpus.rs` holds
the exact bytes of every message and fails if any of them move. The one break
the floor cannot turn into a clean refusal is a new `TileEncoding`, a `u8`
nested inside `ScreenUpdate` with its own tag space and no skip rule; put one
behind a feature bit. `message.rs` argues that case in full.

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
- **Nothing needs a C library installed.** x11rb, not xlib; PAM is `dlopen`ed
  at runtime (`daemon/pam.rs`) so one binary works with or without PAM present.
  Keep new dependencies in that spirit. The updater's TLS is rustls with
  bundled roots for the same reason — there is no system library to find. The
  tree is not *pure* Rust, though, and never was: `zstd-sys` compiles vendored
  libzstd into both binaries and `ring` brings C and assembly in under `ureq`,
  so a build host needs a C compiler. Vendored C a crate builds for itself is
  fine; a library the operator has to install is not.
- **The updater matches release assets by suffix.** `update::asset_suffix`
  looks for `-linux-x86_64.tar.gz`, `-windows-x86_64.zip`,
  `-windows-x86_64-setup.exe` and so on, where the platform names come from
  the matrix in `release.yml`. Renaming an asset in `package-client.sh`, or
  dropping `SHA256SUMS` from the release job, does not fail any build: it
  makes every deployed client report "no published download for this
  platform" instead. Change one, change `update/mod.rs`.
- **`LYNXRDP_RELEASE_TAG` is how a build knows which release it is.**
  `release.yml` sets it, `build.rs` bakes it in, and a build without it
  refuses to replace itself. Do not try to derive this from
  `CARGO_PKG_VERSION`: the workspace version stays `0.1.0` across every
  candidate, so `v0.1.0-rc.6` would compare as *older* than a build that
  reported `0.1.0`. To exercise the updater locally, build with a fake older
  tag: `LYNXRDP_RELEASE_TAG=v0.1.0-rc.1 cargo run -p lynxrdp-client --bin lynxrdp`.
- **`ureq` is pinned to `~3.2` to hold the 1.80 `rust-version`.** 3.4 raises
  its own floor to 1.85. Lift both together or neither; with `resolver = "2"`
  the version choice is not MSRV-aware, so a bare `"3"` silently breaks the
  promise the manifest makes.

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
  fails. Use `if`. The packaging scripts are shellcheck-clean by convention and
  not by enforcement: no workflow runs it, so only review catches a regression.
  `check-glibc-floor.sh` already breaks the rule and gets away with it — the
  loop is a non-final pipeline stage whose exit status is discarded — so tidy
  the style if you are in the file, but do not go hunting for a failure that
  cannot happen.
- **The Windows client is built for the GUI subsystem**
  (`#![windows_subsystem = "windows"]`), so it does not flash a console from
  Explorer. `console.rs` reattaches to the parent terminal to keep the command
  line working — if you add early output, make sure it still lands there.
- Clipboard file lists have no cross-platform crate: X11 `text/uri-list`,
  Windows `CF_HDROP`, macOS `NSPasteboard`, three implementations in
  `fileclip.rs` behind one interface. A change to one usually needs all three.
  The *contents* behind those lists are a second set of three, in the separate
  `crates/filecopy`: a FUSE mount on Linux, an `IDataObject` serving virtual
  `FileContents` on Windows, a loopback WebDAV server on macOS. The client
  publishes session files to the local desktop through it, and the session
  publishes client-copied files into X11 through the same type
  (`session/lazy_clipboard.rs` is one `pub use`), so a change there can land in
  two crates at once.

## Releases

Pushing a tag matching `v*` is the normal way to cut one — the workflow builds
every artifact and creates the GitHub release:

```bash
git tag -a v0.1.0-rc.4 -m "LynxRDP v0.1.0-rc.4"
git push origin v0.1.0-rc.4
```

**A hyphen in the tag marks it a prerelease** (`contains(tag, '-')` in
`release.yml`), so `v0.1.0-rc.4` is a prerelease and `v0.1.0` is not. Check the
existing tags before picking a name; releases so far are `v0.1.0-rc.1` onward.

The workflow also accepts a `workflow_dispatch` input that **creates the tag
itself** (Actions → Release → Run workflow → `tag: v0.1.0-rc.4`). That is the
fallback for an environment where pushing a tag ref is not permitted — some
sandboxed sessions hold a write grant scoped to their working branch and get an
HTTP 403 on the tag push. Reach for it only after a real tag push fails; it is
not the preferred route.

A pushed tag ships the commit the tag points at; a dispatch ships the head of
the ref you dispatch against (`main`) and creates the tag there.

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
