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
  onto the session's upload directory. Parent components are opened relative
  to directory handles with `O_NOFOLLOW`, and the parent handle is retained
  through atomic publication, so a symlink cannot redirect the write.
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

  Anything a transfer holds in memory rather than streaming to a file is
  bounded twice over, by `MAX_MEMORY_TRANSFER_SIZE` and by how many such
  transfers may run at once. Both are enforced in the transfer manager
  rather than in each side's policy: there are two policies, and a rule
  that has to be repeated on both ends of a protocol is one that will
  eventually be applied on only one of them.

  The screen dimensions in `ServerHello` and `ScreenResized` are checked
  against `MAX_SCREEN_DIM` before they reach an allocation. They are two
  `u16`s in a very small message, and unchecked they are a 17 GB request.

None of this replaces trusting the server: a session you log in to runs
your own code as you. The boundary these rules defend is the *client's*
filesystem against a server that turns out to be hostile, and the server's
upload directory against a malformed name.

## File publication and session administration

Transfers preserve existing destination files unless replacement is explicitly
chosen. Files are staged beside the destination and only renamed after all
promised bytes have arrived. Failure and cancellation remove the staged file
when the file worker can run; killing the entire process may leave a temporary
file. A successful rename is atomic visibility, not a power-loss durability
guarantee. The server refuses symlink parent components; replacing a final
symlink replaces the link itself rather than writing its target.

The session list and termination helpers run through SSH as the authenticated
user and inspect only that user's processes. Termination requires both PID and
process start token, rechecks ownership and executable identity, and signals a
pidfd. It provides no cross-user administration privilege and opens no network
listener. Reconnect retains the existing loopback-only SSH transport.

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
  leave the machine. They are sealed (see below), which stops a casual
  capture reading them, but the traffic pattern itself still says a LynxRDP
  host is here and talking to that collector. Keep it on a management network
  or a VPN rather than pointing it across the public internet.
* **Reports are obfuscated, not confidential.** Each datagram is sealed with
  ChaCha20-Poly1305 under a key derived from constants compiled into
  `lynxrdpd` and written in `crates/server/src/reporting/seal.rs`. That means
  a packet capture no longer reads as a list of hostnames and addresses,
  which is what this was added to prevent.

  It does **not** mean the reports are secret. The key ships in the binary
  and in this repository: anyone who can run `strings` on `lynxrdpd`, or read
  the source, can recover it and then decrypt every report and forge
  convincing ones. Treat the sealing as a lock on a garden gate -- it stops
  the casual passer-by looking in, and stops nobody who wants in.

  If reports ever need to withstand someone who has the software, the baked
  constants must become a per-deployment key that is not in the repository.
  The wire format carries a version byte so that change does not have to be a
  flag day.
* **So nothing meaningfully authenticates a report either.** Because the key
  is not secret, a forged report is indistinguishable from a real one. The
  viewer shows claims, not facts. Do not make an access decision from that
  list.
* **It is not a control channel.** Reports flow one way. Nothing the
  monitoring server sends can reach the daemon, because the daemon never
  reads from the socket.

The viewer treats every datagram as hostile input. It is unsealed first --
anything that fails the authentication tag is dropped without further
inspection -- and what survives is then size-capped, strictly parsed, and
stripped of control characters before it reaches a widget, so a malformed or
malicious report cannot corrupt the display.

If you do not need any of this, leave `reporting.enabled = false`, which is
the default, and the code never runs.

## Client updates

The connection manager can check `api.github.com` for a newer release and
replace itself with it. Three things are worth stating plainly.

* **It is the client only, never the server.** `lynxrdpd` runs as root and is
  installed from a package; a root daemon that downloads its own replacement
  would be a far larger thing to trust than a desktop application swapping a
  file in a directory its own user can write. There is no code path that
  updates the server, and a client installed from a `.deb` or `.rpm` refuses
  to update itself too, because those files belong to the package manager.
* **What it verifies is integrity, not authorship.** The download is checked
  against the `SHA256SUMS` published in the same release, which catches a
  truncated or corrupted file. That file arrives from the same host over the
  same connection as the asset, so it is not a second opinion about who
  published the release. Nothing here is code signed. The trust anchor is the
  TLS connection to GitHub and GitHub's own control of the repository — the
  same anchor as downloading the release by hand, and no more.

  Where a stronger claim is wanted, the shape of the code allows it:
  `update::verify` is the only door the downloaded bytes come through, and a
  signature check belongs there.
* **It is outbound, small, and optional.** One HTTPS GET a day at most,
  carrying the client's address and a user agent naming its release. It binds
  nothing, opens nothing, and reads no reply beyond the release listing.
  `Help → Check Automatically` turns it off, which is recorded as
  `updates.check = false` in `settings.toml`; with it off nothing is sent
  unless a user asks for a check.

An update is offered but never applied on its own: downloading and replacing
happen on a click, and the replaced build is only running after a restart.

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
