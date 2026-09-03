# LynxRDP Monitor

A small PySide6 viewer for the heartbeats LynxRDP servers send when
`[reporting]` is enabled. It shows which machines are up and lets you copy an
address straight into an `ssh` or `lynxrdp` command.

```
┌──────────────────────────────────────────────────────────────────────┐
│ Node        │ IP address │ Port │ Sessions │ Version       │ Last seen│
│ desk01      │ 10.0.0.5   │ 3390 │ 2        │ LynxRDP/0.1.0 │ 4s       │
│ build02 *   │ 10.0.0.9   │ 3390 │ 0        │ LynxRDP/0.1.0 │ 3m       │
│ old-box     │ 10.0.0.14  │ 3390 │ 1        │ LynxRDP/0.1.0 │ 9m (stale)│
└──────────────────────────────────────────────────────────────────────┘
```

## Running it

```sh
pip install -r requirements.txt
./lynxrdp-monitor                      # listens on 0.0.0.0:9999
./lynxrdp-monitor --port 9999 --bind 10.0.0.2
./lynxrdp-monitor --stale-after 300    # mark a host stale after 5 minutes
```

Then point servers at it in `/etc/lynxrdp/lynxrdp.toml`:

```toml
[reporting]
enabled = true
destination = "10.0.0.2:9999"
interval_secs = 60
```

## Copying an address

Any of these copies the selected host's IP:

* the **Copy IP address** button
* **Ctrl+C**
* **double-clicking** the row
* **right-click → Copy IP address**

Right-click also offers **Copy ssh command**, which puts `lynxrdp <ip>` on the
clipboard ready to paste, and **Remove from list** for a host you have
retired. A removed host reappears if it reports again.

## What the display means

| Thing | Meaning |
| --- | --- |
| `*` after a name | The host reports one address but its packets arrive from another. Normal behind NAT; see the tooltip for both. |
| *italic* + `(stale)` | Nothing heard for longer than `--stale-after`. The row stays so you can see a machine has gone quiet. |
| `Last seen` | Measured on **this** machine's clock, from when the packet arrived — never from the timestamp inside it, which is the sender's claim. |
| `Sessions` | Desktop sessions running on that host when it reported. |

Hosts are keyed by name, so a machine that changes address moves rather than
appearing twice. Two machines sharing a hostname will collide — that is a
naming problem worth seeing rather than hiding.

## What this is not

Reports are **unauthenticated UDP**. Anyone who can reach this port can send
one, and anything shown here is a claim, not a fact. Treat the list as a
convenience for finding your own machines, not as an inventory you would make
a security decision from. Run it on a management network. See the project's
`SECURITY.md`.

Nothing is stored: the list is built from what arrives while the viewer runs,
and starts empty each time.

## Tests

```sh
pip install pytest PySide6
python -m pytest tests/            # GUI tests run headless via QT_QPA_PLATFORM=offscreen
```

`tests/test_model.py` covers parsing and bookkeeping and needs no Qt;
`tests/test_gui.py` drives the real window, including that malformed
datagrams cannot disturb it. Both run without a display.
