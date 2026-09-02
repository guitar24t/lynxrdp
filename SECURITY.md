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

The protocol parser is written without `unsafe`, bounds-checks every
length, caps message sizes and is fuzzed with property tests against
random input.

## Hardening tips

* Keep `PermitOpen` in `sshd_config` at its default or restrict it to
  `127.0.0.1:3390` for LynxRDP-only accounts.
* Use `access.allow_groups = ["lynxrdp"]` to limit who can start desktops.
* Set `session.idle_timeout_secs` so abandoned sessions do not linger.
* The daemon has no network exposure, so the remaining attack surface is
  SSH itself: use keys, disable password authentication, keep OpenSSH
  updated.

## Reporting

Please report security issues privately to the repository owner rather
than in public issues.
