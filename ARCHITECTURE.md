# Architecture

## Processes

```
lynxrdpd (root)
 ├─ listens 127.0.0.1:3390 (and optionally a Unix socket)
 ├─ identifies the peer uid: /proc/net/tcp for TCP, SO_PEERCRED for Unix
 ├─ access policy (min uid, allow/deny lists, groups)
 ├─ handoff worker pool (one connection per uid at a time)
 └─ per user: lynxrdpd --supervise (root)
              ├─ pam_open_session("lynxrdp")   ← systemd-logind session, XDG_RUNTIME_DIR, limits
              └─ lynxrdp-session (setuid user, own session id)
                   ├─ Xvfb -auth <private cookie> -displayfd …
                   ├─ startwm.sh → the desktop environment
                   ├─ core thread: capture, encode, input, cursor, clipboard, flow control
                   └─ control socket: accepts client fds handed over by lynxrdpd
```

Only the first three lines run on the listening thread. They cost
microseconds, and the `/proc/net/tcp` lookup has to happen there because it
needs the peer's socket to still be in the kernel's table. The handoff itself
does not: a cold start budgets 45 seconds for Xvfb, and a session its owner
has stopped never answers at all, so it runs on a pool of workers. The pool
admits one connection per uid at a time -- two spawning at once would leave
one supervisor holding an X server on a socket the other had just unlinked --
and refuses rather than queues when it is full.

The daemon never touches pixels or input. Once it has handed the client
socket to the session process (`SCM_RIGHTS` over a root-only Unix socket in
`/run/lynxrdp/sessions/`), it is out of the picture. A daemon restart
therefore does not affect running sessions; the new daemon finds the
control sockets and reuses them.

`lynxrdp-session` can also be run directly by a user with `--listen`; it
then performs the same `/proc/net/tcp` uid check itself.

## Session core

One thread owns the X connection objects and all decisions. It selects over
three channels: X events (forwarded by a reader thread), client messages
(forwarded by a per-connection reader thread) and a housekeeping tick.

Frame pipeline:

1. `DamageNotify` sets a dirty flag. Nothing is fetched yet.
2. When a frame is allowed (client acknowledged enough frames, frame
   interval elapsed), the accumulated damage region is fetched
   (`XDamageSubtract` + `XFixesFetchRegion`), coalesced into tile-aligned
   rectangles, and captured with `XShmGetImage` (one request per
   rectangle, or the whole screen when damage is widespread).
3. Before diffing, the encoder looks for a vertical translation between
   the reference frame and the new one (row hashes propose a shift, then
   the pixels are compared to confirm it). A match becomes a `CopyRect`:
   the client moves pixels it already holds, so scrolling costs a dozen
   bytes instead of a screenful.

   The search runs **per damage region, never over their union**. A union
   spanning a scrolling pane and an unrelated change beside it contains
   rows that moved for two different reasons, and then no single shift
   explains any of them -- folding does not weaken detection, it ends it.
   Rows that did not change are compared before anything is hashed, which
   is what keeps an idle screen from paying for the search.

   Every copy found across every region is applied to the reference in
   **one batch**. The decoder reads all sources before writing any
   destination, so applying them one at a time would leave the encoder's
   reference holding a different picture from the client's framebuffer
   wherever two copies overlap -- silently, permanently, and invisibly to
   the tile pass that is supposed to catch differences.
4. The encoder then compares each 64×64 tile against the reference frame
   and trims to the bounding box of changed pixels. Each tile is stored
   either as packed 24-bit RGB or, when it has at most 256 distinct
   colours, as a palette plus 1/2/4/8-bit indices — whichever is smaller —
   and then compressed with LZ4 or Zstd if that shrinks it further. A
   single-colour tile short-circuits to three bytes.

   The codec is lossless throughout: the client's framebuffer is always
   bit-identical to the server's, which is what keeps text crisp and lets
   the property tests assert exact round-trips.
5. The copies and tiles are batched into one `ScreenUpdate`, which is
   queued to the writer thread. `max_in_flight`
   (default 2) frames may be unacknowledged; further damage accumulates
   and is sent as one frame after the next ack. The client therefore never
   has a backlog of stale frames.

Input is applied the moment it arrives, ahead of frame work. Pointer motion
is injected with `XTestFakeInput`; keys are mapped from keysyms using the
server's keyboard mapping, with temporary Shift/AltGr presses when the
client's layout differs, and dynamic binding of unmapped keysyms to spare
keycodes (how `xdotool` types Unicode).

The cursor is not captured into frames: XFIXES cursor images are sent when
the shape changes and the client draws the pointer itself at its local
mouse position, so pointer feedback is independent of network latency.

## Protocol

Length-prefixed binary messages (see `crates/proto/src/message.rs`),
little endian, no TLS (the SSH tunnel provides confidentiality and
integrity). Handshake: `ClientHello` → `ServerHello | Rejected` → full
`ScreenUpdate`. Everything is versioned by a single `PROTOCOL_VERSION`.

Message tags below 128 are **structural**: a peer that cannot decode one
cannot stay in sync, so an unknown tag there is fatal. Tags at or above 128
are **skippable extensions**, discarded whole by the framing layer, which
costs nothing because the length prefix already says how much to throw
away. That range exists so a newer peer can send an optional message
without dropping the connection to an older one -- its value is entirely in
the *older* peer, which is why it is worth having before anything uses it.

Pixels travel as tightly packed 24-bit RGB. The client keeps a framebuffer
in `0x00RRGGBB` and presents it with `softbuffer`, which maps directly to
the window system's native format on all three platforms.

## Transfer channel

Clipboard images, clipboard file copies and explicit file transfers all
share one mechanism (`crates/proto/src/transfer.rs`): an offer the peer may
accept or refuse, then numbered 64 KiB chunks, then an end marker. Both
directions run over the same connection as the screen stream, so it must
never stall interaction:

* Chunks are capped at `CHUNK_SIZE`, so a frame waits for at most one chunk
  to drain rather than a whole file.
* The sender keeps at most `WINDOW_CHUNKS` chunks of any one transfer
  unacknowledged, which bounds how much of a large file can be in flight.
* `GLOBAL_WINDOW_CHUNKS` bounds the total across *all* transfers. The
  per-transfer window alone says nothing about how many transfers there
  are, and staging a clipboard file copy fans out one per file -- so the
  guarantee above is about the queue only because this third bound exists.
* Transfers that land in memory rather than on disk are capped separately,
  in size and in number: `MAX_TRANSFER_SIZE` is the right bound for
  something streaming to a file and the wrong one for a `Sink::Memory`.

A receiver decides what to do with each offer through a `TransferPolicy`,
which returns either an in-memory sink or a file. That is the single place
each side enforces what it is willing to receive, so the rules live in one
readable function per side rather than spread through the message loop.

Clipboard contents are fetched on demand: an offer names the formats
available, and the bytes only move if the other side asks for them. Copying
a large image you never paste therefore costs nothing.

## File I/O and clipboard batches

The session's bounded file worker owns all file handles. The core submits jobs
without waiting for a queue slot; overload fails a transfer rather than stopping
input and capture. Read adapters return `WouldBlock` until data arrives, preserving
partial chunks in the transfer sender. Final publication is also polled: the
receiver retains its sink until the worker confirms the rename, then reports
success. Worker events wake the core, with housekeeping retries and a timeout
for stalled reads and publication. Pending opens are capped at 64 per session.
Dropping an adapter cancels queued work without sending a blocking cleanup job;
the worker releases cancelled handles when it can next run.

Both sides use `proto::clipboard_batch::ClipBatch` for unique cross-platform
names, eight concurrent file requests, cancellation of superseded batches, and
settling every success or failure. A partial batch publishes the successful
files and reports the missing ones. Disconnect clears transfer state but keeps
the server's ID allocator advancing. Open callbacks are generation-checked
before their pending entry is removed.

`proto::atomic_file` stages on the destination filesystem and publishes only on
successful completion. The server resolves upload parents using directory
handles and `openat(O_NOFOLLOW)`, then retains the parent handle through the
rename. Both sides preserve existing destinations unless explicitly instructed
to replace them. The `ATOMIC_FILES` feature bit negotiates the optional
`TransferOptions` message (extension tag 129); every existing wire layout and
the protocol floor remain unchanged. Unknown extensions are still skipped;
known extensions are decoded normally.

Session administration uses a separate SSH command, not a desktop connection.
`lynxrdp-session --list-sessions` inspects only the caller's process ownership
and session executable identity. Termination opens a pidfd and rechecks the
process start token and owner before sending SIGTERM, avoiding PID reuse.
The launcher performs these commands on a worker and exposes reconnect and
confirmed termination in its Running Desktops window.

## Client

`crates/client/src/connection.rs` is a UI-free protocol client used by the
GUI and by the tests. `app.rs` is a winit `ApplicationHandler` on top of it.
`tunnel.rs` runs the system `ssh` with `-N -L … -o ExitOnForwardFailure=yes`
and waits for the local port to accept connections (which OpenSSH only does
after authentication succeeded).

The binary has two entry points and one process model. Started with a
destination it opens a session window directly; started with no arguments it
opens the connection manager (`launcher.rs`, an egui window over
`profiles.rs`), and **connecting re-invokes the same executable as a child
process** (`launch.rs`).

That split is forced and then useful. Forced, because a process may hold only
one winit event loop, and the launcher already holds one — a session cannot
open its window in the launcher's process. Useful, because several sessions
can then run at once, a session that dies cannot take the manager with it,
and the argument list a saved connection produces is exactly what a user
could have typed, so the GUI and the command line cannot drift apart.

`profiles.rs` holds no credentials of any kind. SSH owns authentication, and
a second, weaker copy of it on disk would be a liability with no benefit.

`update/` is the connection manager's self-updater, and it lives on the
launcher side only — a session window is a child process with no menus, and
replacing an executable under a desktop that is mid-keystroke would be rude.
Its shape follows the same rule as the rest of the client: everything that
decides anything is a pure function taking its inputs as arguments — which
release, which asset, whether this installation may be replaced and how —
because none of the interesting cases (a `.deb` install, an Intel Mac with no
download, a working copy) exist on the machine running the tests. Only
`update/fetch.rs` and `update/install.rs` touch the world, and the swap they
perform always unpacks to a staging name on the target's own filesystem and
then renames, so an interrupted update leaves a working application and some
rubbish beside it rather than half an executable.

The version a build believes it is comes from `LYNXRDP_RELEASE_TAG`, stamped
in by `build.rs` from the release workflow. The Cargo version cannot answer
that question: it stays `0.1.0` across every release candidate, so a build
from `v0.1.0-rc.5` would read itself as `0.1.0` and conclude that
`v0.1.0-rc.6` was a downgrade. A build without the tag is not a release build
and will not replace itself.

## Monitoring reports

`crates/server/src/reporting.rs` is optional and off by default. When it is
on, the daemon sends one JSON datagram per interval to a monitoring server.

It runs on its own thread rather than in the accept loop, for one reason:
resolving a name can block for seconds and the accept loop cannot afford to
wait. The thread reads the live session count through an `AtomicUsize`, so it
never contends with connection handling.

The source address in the report is not guessed. The thread connects a UDP
socket to the destination and asks the kernel what local address it chose,
which is the address the monitoring server will actually see -- correct on a
multi-homed host, where picking the "first" interface would not be.

UDP is deliberate: a monitoring server that is down, slow or absent must cost
the daemon nothing, and a lost report costs one interval of staleness.

`reporting/seal.rs` wraps each datagram in ChaCha20-Poly1305 under a key
derived from baked-in constants, mirrored by the viewer's `crypto.py`. The
two are pinned to each other by a known-answer test on the derived key on
both sides, plus a datagram captured from a real server that the Python tests
must be able to open -- drift between the implementations fails a test rather
than silencing every deployed viewer at once. The format carries a magic and
a version byte, authenticated as associated data, so a later change to the
scheme need not be a flag day.

## Testing strategy

* Pure logic (codec, framing, key mapping, `/proc/net/tcp` parsing, access
  policy, config) has unit and property tests.
* `crates/server/tests/e2e.rs` starts real `lynxrdp-session` processes on
  Xvfb and drives them with the headless client: first frame, incremental
  updates, keyboard (verified with an in-process X11 window that receives
  the events), pointer (`xdotool`), resize (RandR), clipboard text, images
  and file copies (`xclip`), file upload and download including refusal of
  a path-traversing destination, reconnection, lifecycle.
* `crates/server/tests/daemon.rs` runs `lynxrdpd --allow-non-root` and
  checks identification, handoff, session reuse and policy rejections.
* CI builds the client on Windows, macOS (Apple Silicon) and Linux
  (x86_64, aarch64), builds the `.deb`/`.rpm`, installs the `.deb` and
  checks the service starts.
* The Windows and macOS clipboard file backends are the one part CI cannot
  exercise fully: they are compiled for their targets under
  `clippy -D warnings`, and the CF_HDROP block builder is pure and unit
  tested, but pasting into a real Explorer or Finder is a manual check.
