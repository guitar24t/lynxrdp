//! Optional heartbeat reports to a monitoring server.
//!
//! When `[reporting]` is enabled the daemon sends a small JSON datagram to a
//! monitoring server every `interval_secs`, saying which host it is and where
//! it can be reached. `tools/lynxrdp-monitor` is a viewer for these.
//!
//! Three properties are deliberate:
//!
//! * **Outbound only.** No socket is bound to a wildcard address and nothing
//!   is read back, so enabling this does not make the host reachable in any
//!   way it was not already. The security model in SECURITY.md is unchanged.
//! * **UDP, and unacknowledged.** A monitoring server that is down, slow or
//!   missing must never slow the daemon down or hold up a connection. A lost
//!   report costs one interval of staleness and nothing else.
//! * **On its own thread.** Name resolution can block for seconds, and the
//!   accept loop cannot afford to. The thread reads the live session count
//!   through an atomic, so it never takes a lock the accept loop holds.
//!
//! The payload is not authenticated or encrypted. See SECURITY.md.

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

use crate::config::Config;

/// Largest datagram we will build. Well under any sane MTU, so a report is
/// never fragmented; the fields are all short and bounded.
pub const MAX_REPORT_BYTES: usize = 1200;

/// Longest hostname or node name we will report, in bytes.
pub const MAX_NAME_BYTES: usize = 253;

/// Split a `host:port` destination, keeping IPv6 literals in brackets intact.
///
/// Returns the host and port separately rather than a `SocketAddr` because
/// resolution is deliberately deferred to the reporter thread.
pub fn split_destination(dest: &str) -> Result<(String, u16)> {
    let dest = dest.trim();
    if dest.is_empty() {
        bail!("destination is empty");
    }
    let (host, port) = if let Some(rest) = dest.strip_prefix('[') {
        // [::1]:9999
        let end = rest.find(']').context("unterminated IPv6 literal")?;
        let after = &rest[end + 1..];
        let port = after
            .strip_prefix(':')
            .context("IPv6 destination needs a :port")?;
        (rest[..end].to_string(), port)
    } else {
        let (h, p) = dest
            .rsplit_once(':')
            .context("destination must be host:port")?;
        if h.contains(':') {
            bail!("IPv6 destinations must be written as [address]:port");
        }
        (h.to_string(), p)
    };
    if host.is_empty() {
        bail!("destination has no host");
    }
    let port: u16 = port
        .parse()
        .with_context(|| format!("bad port {port:?} in destination"))?;
    if port == 0 {
        bail!("destination port must not be 0");
    }
    Ok((host, port))
}

/// What one heartbeat says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Host this is about, as the operator should see it.
    pub node: String,
    /// Address the monitoring server would see us come from.
    pub ip: String,
    /// Port the daemon serves on, so an operator knows what to forward.
    pub port: u16,
    /// Server version string.
    pub version: String,
    /// Sessions running right now.
    pub sessions: usize,
    /// Seconds this daemon has been up.
    pub uptime_secs: u64,
    /// Unix time the report was built, for staleness in the viewer.
    pub time: u64,
}

impl Report {
    /// Serialise as a single-line JSON object.
    ///
    /// Hand-written rather than pulling in a JSON crate: the shape is fixed
    /// and flat, and `escape` below is the only part that needs care.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(256);
        out.push('{');
        out.push_str("\"node\":");
        escape(&self.node, &mut out);
        out.push_str(",\"ip\":");
        escape(&self.ip, &mut out);
        out.push_str(",\"port\":");
        out.push_str(&self.port.to_string());
        out.push_str(",\"version\":");
        escape(&self.version, &mut out);
        out.push_str(",\"sessions\":");
        out.push_str(&self.sessions.to_string());
        out.push_str(",\"uptime_secs\":");
        out.push_str(&self.uptime_secs.to_string());
        out.push_str(",\"time\":");
        out.push_str(&self.time.to_string());
        out.push('}');
        out
    }
}

/// Append `s` to `out` as a quoted JSON string.
///
/// Escapes what RFC 8259 requires: quote, backslash and everything below
/// 0x20. Anything else, including non-ASCII, is emitted as UTF-8, which JSON
/// allows and every parser accepts.
fn escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// This machine's hostname, or `"unknown"` if it cannot be read.
pub fn hostname() -> String {
    match std::fs::read_to_string("/proc/sys/kernel/hostname") {
        Ok(s) => {
            let name = s.trim();
            if name.is_empty() {
                "unknown".to_string()
            } else {
                truncate_name(name)
            }
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Keep a name within [`MAX_NAME_BYTES`] without splitting a character.
pub fn truncate_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_string();
    }
    let mut end = MAX_NAME_BYTES;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_string()
}

/// Handle on the reporter thread.
pub struct Reporter {
    sessions: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Reporter {
    /// Start reporting if the configuration asks for it.
    ///
    /// Returns `Ok(None)` when reporting is disabled. A destination that
    /// cannot be parsed is an error; one that cannot be *resolved* is not,
    /// because the monitoring server may simply not be up yet.
    pub fn start(cfg: &Config) -> Result<Option<Self>> {
        if !cfg.reporting.enabled {
            return Ok(None);
        }
        let (host, port) = split_destination(&cfg.reporting.destination)?;
        let node = match &cfg.reporting.node_name {
            Some(n) => truncate_name(n.trim()),
            None => hostname(),
        };
        let interval = Duration::from_secs(cfg.reporting.interval_secs);
        let listen_port = cfg.listen.port;
        let sessions = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let sessions = Arc::clone(&sessions);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("lynxrdp-report".into())
                .spawn(move || {
                    run(&host, port, node, listen_port, interval, &sessions, &stop);
                })
                .context("spawning the reporting thread")?
        };
        log::info!(
            "reporting to {} every {}s",
            cfg.reporting.destination,
            cfg.reporting.interval_secs
        );
        Ok(Some(Self {
            sessions,
            stop,
            thread: Some(thread),
        }))
    }

    /// Tell the reporter how many sessions are running.
    ///
    /// Called from the accept loop, so it must stay a single atomic store.
    pub fn set_sessions(&self, n: usize) {
        self.sessions.store(n, Ordering::Relaxed);
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Resolve `host:port` to one address, preferring whatever the resolver put
/// first. Returns `None` when the name does not resolve right now.
fn resolve(host: &str, port: u16) -> Option<SocketAddr> {
    match (host, port).to_socket_addrs() {
        Ok(mut addrs) => addrs.next(),
        Err(e) => {
            log::debug!("cannot resolve {host}:{port} yet: {e}");
            None
        }
    }
}

/// Send one report, returning the address it went to so the caller can log a
/// change. Errors are logged at debug: a monitoring server being unreachable
/// is expected and must not fill the journal.
fn send_once(
    dest: SocketAddr,
    node: &str,
    listen_port: u16,
    sessions: usize,
    started: Instant,
) -> Result<()> {
    // Bind the family the destination needs, then connect so the kernel
    // picks the source address it would actually route from. That is what we
    // report, which is what makes this correct on a multi-homed host.
    let bind: SocketAddr = if dest.is_ipv4() {
        ([0, 0, 0, 0], 0).into()
    } else {
        (std::net::Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let sock = UdpSocket::bind(bind).context("binding a reporting socket")?;
    sock.connect(dest)
        .with_context(|| format!("connecting a reporting socket to {dest}"))?;
    let ip = sock
        .local_addr()
        .context("reading the local address")?
        .ip()
        .to_string();

    let report = Report {
        node: node.to_string(),
        ip,
        port: listen_port,
        version: crate::SERVER_NAME.to_string(),
        sessions,
        uptime_secs: started.elapsed().as_secs(),
        time: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    };
    let body = report.to_json();
    if body.len() > MAX_REPORT_BYTES {
        // Cannot happen with bounded names, but truncating a datagram would
        // produce invalid JSON at the far end, so refuse rather than send.
        bail!(
            "report is {} bytes, over the {MAX_REPORT_BYTES} cap",
            body.len()
        );
    }
    sock.send(body.as_bytes())
        .with_context(|| format!("sending a report to {dest}"))?;
    Ok(())
}

/// The reporter thread: send, then wait out the interval in small steps so
/// shutdown does not have to wait for a whole one.
fn run(
    host: &str,
    port: u16,
    node: String,
    listen_port: u16,
    interval: Duration,
    sessions: &AtomicUsize,
    stop: &AtomicBool,
) {
    let started = Instant::now();
    let step = Duration::from_millis(200).min(interval);
    while !stop.load(Ordering::SeqCst) {
        match resolve(host, port) {
            Some(dest) => {
                if let Err(e) = send_once(
                    dest,
                    &node,
                    listen_port,
                    sessions.load(Ordering::Relaxed),
                    started,
                ) {
                    log::debug!("report to {dest} failed: {e:#}");
                }
            }
            None => log::debug!("{host}:{port} does not resolve; skipping this report"),
        }
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(step);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_host_and_port() {
        assert_eq!(
            split_destination("monitor.example.org:9999").unwrap(),
            ("monitor.example.org".to_string(), 9999)
        );
        assert_eq!(
            split_destination(" 10.0.0.5:1 ").unwrap(),
            ("10.0.0.5".to_string(), 1)
        );
    }

    #[test]
    fn splits_ipv6_literals() {
        assert_eq!(
            split_destination("[::1]:9999").unwrap(),
            ("::1".to_string(), 9999)
        );
        // A bare IPv6 address is ambiguous with host:port, so it is refused
        // rather than silently read as host "::" with port "1".
        let err = split_destination("::1:9999").unwrap_err().to_string();
        assert!(err.contains("[address]:port"), "{err}");
    }

    #[test]
    fn rejects_malformed_destinations() {
        for bad in [
            "",
            "   ",
            "nocolon",
            "host:",
            "host:0",
            "host:70000",
            "host:notaport",
            ":9999",
            "[::1]9999",
            "[::1",
        ] {
            assert!(
                split_destination(bad).is_err(),
                "{bad:?} should have been refused"
            );
        }
    }

    fn sample() -> Report {
        Report {
            node: "desk01".into(),
            ip: "10.0.0.5".into(),
            port: 3390,
            version: "LynxRDP/0.1.0".into(),
            sessions: 2,
            uptime_secs: 3600,
            time: 1_756_900_000,
        }
    }

    #[test]
    fn json_has_the_documented_shape() {
        assert_eq!(
            sample().to_json(),
            concat!(
                r#"{"node":"desk01","ip":"10.0.0.5","port":3390,"#,
                r#""version":"LynxRDP/0.1.0","sessions":2,"#,
                r#""uptime_secs":3600,"time":1756900000}"#
            )
        );
    }

    #[test]
    fn json_escapes_awkward_names() {
        // A hostname will not contain these, but node_name is free text from
        // the config file, and a broken datagram is worse than an ugly one.
        let mut r = sample();
        r.node = "a\"b\\c\nd\te\u{1}f".into();
        let json = r.to_json();
        assert!(json.contains(r#"\"b"#), "{json}");
        assert!(json.contains(r#"\\c"#), "{json}");
        assert!(json.contains(r#"\nd"#), "{json}");
        assert!(json.contains(r#"\te"#), "{json}");
        // The control character must arrive as a JSON escape, never raw.
        // Built from a code point so this file holds no stray control byte.
        let backslash = char::from_u32(92).unwrap();
        assert!(json.contains(&format!("{backslash}u0001")), "{json}");
        assert!(!json.contains(char::from_u32(1).unwrap()), "{json}");
    }

    #[test]
    fn json_keeps_non_ascii_as_utf8() {
        let mut r = sample();
        r.node = "b\u{fc}ro-01".into();
        assert!(r.to_json().contains("b\u{fc}ro-01"));
    }

    #[test]
    fn names_are_truncated_on_a_character_boundary() {
        // Every char here is two bytes, so the cap lands mid-character unless
        // the boundary is respected; slicing there would panic.
        let long = "\u{e9}".repeat(MAX_NAME_BYTES);
        let cut = truncate_name(&long);
        assert!(cut.len() <= MAX_NAME_BYTES);
        assert!(long.starts_with(&cut));
        assert!(!cut.is_empty());
        assert_eq!(truncate_name("desk01"), "desk01");
    }

    #[test]
    fn a_report_fits_in_one_datagram() {
        // The worst case the caps allow.
        let r = Report {
            node: "n".repeat(MAX_NAME_BYTES),
            ip: "2001:0db8:85a3:0000:0000:8a2e:0370:7334".into(),
            port: u16::MAX,
            version: "LynxRDP/999.999.999".into(),
            sessions: usize::MAX,
            uptime_secs: u64::MAX,
            time: u64::MAX,
        };
        let n = r.to_json().len();
        assert!(n <= MAX_REPORT_BYTES, "{n} bytes");
    }

    #[test]
    fn hostname_is_never_blank() {
        assert!(!hostname().is_empty());
    }
}
