# Security model

## Threat model

LynxRDP assumes the server is reachable only via SSH and treats the SSH
server as the sole authentication and transport-security boundary.

* The daemon binds loopback addresses only and refuses to start otherwise.
  There is no option to expose it.
* No credentials are transmitted inside the LynxRDP protocol. A client that
  reaches the daemon has already proven its identity to `sshd`.
* Identity is derived from the kernel, not from anything the client says:
  for TCP the daemon reads the owner uid of the peer socket from
  `/proc/net/tcp` (the same mechanism as an ident daemon); for the optional
  Unix socket it uses `SO_PEERCRED`. OpenSSH's post-authentication process,
  which performs port forwards, runs as the authenticated user, so the
  socket's owner is that user.
* A session is created for exactly that uid. A user cannot obtain another
  user's session; there is no username field to tamper with.
* Root and system accounts are refused by default (`access.min_uid`,
  `access.deny_users`), and `allow_users`/`allow_groups` can restrict
  access further. PAM account checks (`pam_acct_mgmt`) also apply, so
  locked or expired accounts cannot open sessions.

## Local users on the server

A local, unprivileged user can also connect to `127.0.0.1:3390` directly.
They get their *own* session, which they are entitled to anyway. They
cannot reach anybody else's session:

* Session control sockets live in a root-only directory
  (`/run/lynxrdp/sessions`, mode 0700).
* Each session's X server is started with a private, randomly generated
  MIT-MAGIC-COOKIE-1 in an authority file readable only by that user. Xvfb
  would otherwise accept any local connection.
* The session process verifies the uid in every handoff it receives and
  that the handoff comes from root or itself.

## Privilege separation

`lynxrdpd` runs as root but does very little: accept, identify, decide,
spawn, hand over. Per session it forks a supervisor that opens the PAM
session and then `exec`s `lynxrdp-session` after `initgroups`/`setgid`/
`setuid` (and verifies that regaining root fails). All X11, image and
protocol parsing code runs with the user's privileges only.

The protocol crate is `#![forbid(unsafe_code)]`, so that is compiler
enforced rather than a convention: every length is bounds-checked, message
sizes are capped before allocation, and random input is thrown at the
parser with property tests. `unsafe` in the rest of the tree is confined to
places that must call C: the daemon's `libc` and PAM calls on the server,
and the Win32 clipboard calls in the client's `fileclip.rs`.

## File transfer and clipboard

Files move over the transfer channel in both directions, so each side
decides for itself what it will accept. Neither side trusts a name or an
offer from the other:

* **Upload destinations are relative and cannot escape.**
  `safe_relative_path` rejects any `..` component, rejects embedded NULs,
  and treats `\` as a separator as well as `/` so a Windows client cannot
  smuggle a path through as one odd filename. The result is always joined
  onto the session's upload directory.
* **Neither side accepts a download it did not ask for.** A transfer is
  only written to disk if its id is already registered as one this side
  requested; anything else is refused as an "unsolicited download". This
  is what stops a compromised session from pushing files onto the client.
* **The client serves only what it published.** When the session asks for
  a file by path, the client answers only for paths it explicitly put on
  the clipboard, so a file request cannot be turned into a read of
  arbitrary client files.
* **Sizes are bounded everywhere.** Transfers are capped at
  `MAX_TRANSFER_SIZE`, clipboard text at 4 MiB, clipboard images at 64 MiB
  and decoded images at `MAX_PIXELS`, so a hostile peer cannot make either
  side allocate without limit. A drag-and-drop is capped at
  `MAX_DROPPED_FILES` and does not follow symlinks.

None of this replaces trusting the server: a session you log in to runs
your own code as you. The boundary these rules defend is the *client's*
filesystem against a server that turns out to be hostile, and the server's
upload directory against a malformed name.

## Monitoring reports

The optional `[reporting]` section is the only part of LynxRDP that speaks to the network of its
own accord, and it is off unless you switch it on. What it does and does not
change:

* **It opens nothing.** The daemon only sends. No socket is bound to a
  wildcard address, no reply is read, and the loopback-only rule above is
  untouched. Enabling reporting does not make the host reachable in any way
  it was not already.
* **It does disclose.** Each interval the hostname, the address the host
  would be reached on, the LynxRDP version and the number of running sessions
  go out in the clear as a UDP datagram. Anyone on the path can read that,
  and it is a tidy map of your estate. Keep it on a management network or a
  VPN rather than pointing it across the public internet.
* **Nothing authenticates it.** There is no signature, so anyone who can
  reach the monitoring port can invent a host, and a real report can be
  forged or suppressed. The viewer shows claims, not facts. Do not make an
  access decision from that list. (An HMAC over the payload would fix this
  and is deliberately not implemented yet; it would need a shared key
  distributed to every server, which is a bigger change than this feature
  warranted.)
* **It is not a control channel.** Reports flow one way. Nothing the
  monitoring server sends can reach the daemon, because the daemon never
  reads from the socket.

The viewer treats every datagram as hostile input: size-capped, strictly
parsed, and stripped of control characters before anything reaches a widget,
so a malformed or malicious report cannot corrupt its display.

If you do not need any of this, leave `reporting.enabled = false`, which is
the default, and the code never runs.

## Hardening tips

* Keep `PermitOpen` in `sshd_config` at its default or restrict it to
  `127.0.0.1:3390` for LynxRDP-only accounts.
* Use `access.allow_groups = ["lynxrdp"]` to limit who can start desktops.
* Set `session.idle_timeout_secs` so abandoned sessions do not linger.
* If `reporting` is on, restrict the monitoring port to the management
  network with a firewall rule; anyone who can reach it can pollute the view.
* The daemon has no network exposure, so the remaining attack surface is
  SSH itself: use keys, disable password authentication, keep OpenSSH
  updated.

## Reporting

Please report security issues privately to the repository owner rather
than in public issues.
