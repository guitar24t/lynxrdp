# Architecture

## Processes

```
lynxrdpd (root)
 ├─ listens 127.0.0.1:3390 (and optionally a Unix socket)
 ├─ identifies the peer uid: /proc/net/tcp for TCP, SO_PEERCRED for Unix
 ├─ access policy (min uid, allow/deny lists, groups)
 └─ per user: lynxrdpd --supervise (root)
              ├─ pam_open_session("lynxrdp")   ← systemd-logind session, XDG_RUNTIME_DIR, limits
              └─ lynxrdp-session (setuid user, own session id)
                   ├─ Xvfb -auth <private cookie> -displayfd …
                   ├─ startwm.sh → the desktop environment
                   ├─ core thread: capture, encode, input, cursor, clipboard, flow control
                   └─ control socket: accepts client fds handed over by lynxrdpd
```

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
3. The encoder compares each 64×64 tile against the reference frame,
   trims to the bounding box of changed pixels, and emits `Solid`, `Lz4`
   or `Raw` tiles.
4. The `ScreenUpdate` is queued to the writer thread. `max_in_flight`
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

Pixels travel as tightly packed 24-bit RGB. The client keeps a framebuffer
in `0x00RRGGBB` and presents it with `softbuffer`, which maps directly to
the window system's native format on all three platforms.

## Client

`crates/client/src/connection.rs` is a UI-free protocol client used by the
GUI and by the tests. `app.rs` is a winit `ApplicationHandler` on top of it.
`tunnel.rs` runs the system `ssh` with `-N -L … -o ExitOnForwardFailure=yes`
and waits for the local port to accept connections (which OpenSSH only does
after authentication succeeded).

## Testing strategy

* Pure logic (codec, framing, key mapping, `/proc/net/tcp` parsing, access
  policy, config) has unit and property tests.
* `crates/server/tests/e2e.rs` starts real `lynxrdp-session` processes on
  Xvfb and drives them with the headless client: first frame, incremental
  updates, keyboard (verified with an in-process X11 window that receives
  the events), pointer (`xdotool`), resize (RandR), clipboard (`xclip`),
  reconnection, lifecycle.
* `crates/server/tests/daemon.rs` runs `lynxrdpd --allow-non-root` and
  checks identification, handoff, session reuse and policy rejections.
* CI builds the client on Windows, macOS (both architectures) and Linux
  (x86_64, aarch64), builds the `.deb`/`.rpm`, installs the `.deb` and
  checks the service starts.
