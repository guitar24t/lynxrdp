//! Server configuration (`/etc/lynxrdp/lynxrdp.toml`).

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Default location of the configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/lynxrdp/lynxrdp.toml";

/// Top-level configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Listener settings.
    pub listen: ListenConfig,
    /// Access control.
    pub access: AccessConfig,
    /// Per-user session settings.
    pub session: SessionConfig,
    /// Optional heartbeat reports to a monitoring server.
    pub reporting: ReportingConfig,
}

/// Where the daemon listens.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ListenConfig {
    /// IP address to bind. Must be a loopback address.
    pub address: IpAddr,
    /// TCP port to bind.
    pub port: u16,
    /// Optional Unix socket path to listen on additionally. Peers are
    /// identified with `SO_PEERCRED`. SSH can forward to it with
    /// `-L 3390:/run/lynxrdp/lynxrdp.sock`.
    pub unix_socket: Option<PathBuf>,
}

/// Who may open sessions.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    /// Users with a uid below this are refused (system accounts and root).
    pub min_uid: u32,
    /// If non-empty only these user names may connect.
    pub allow_users: Vec<String>,
    /// If non-empty only members of these groups may connect.
    pub allow_groups: Vec<String>,
    /// These user names are always refused.
    pub deny_users: Vec<String>,
}

/// Heartbeat reports to a monitoring server.
///
/// This is the one part of LynxRDP that talks to the network on its own, so
/// it is off unless switched on. The daemon only ever *sends*: no port is
/// opened and nothing is accepted in reply, so enabling it does not widen
/// the attack surface of the host. What it does do is put the hostname and
/// address of this machine on the wire in the clear, once per interval --
/// see SECURITY.md before pointing it across an untrusted network.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ReportingConfig {
    /// Whether to send reports at all.
    pub enabled: bool,
    /// Where to send them, as `host:port`. The host may be a name or an
    /// address; names are resolved once when the reporter starts.
    pub destination: String,
    /// Seconds between reports.
    pub interval_secs: u64,
    /// Name to report instead of the system hostname.
    pub node_name: Option<String>,
}

impl Default for ReportingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: String::new(),
            interval_secs: 60,
            node_name: None,
        }
    }
}

/// Session settings (used by `lynxrdpd` when spawning `lynxrdp-session`).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// Initial width when the client does not ask for one.
    pub default_width: u32,
    /// Initial height when the client does not ask for one.
    pub default_height: u32,
    /// Largest width a client may request (also the X server's virtual size).
    pub max_width: u32,
    /// Largest height a client may request.
    pub max_height: u32,
    /// X server executable.
    pub xserver: String,
    /// Extra arguments for the X server.
    pub xserver_args: Vec<String>,
    /// Script/command that starts the desktop environment.
    pub startwm: String,
    /// Upper bound on frames per second sent to the client.
    pub max_fps: u32,
    /// Number of frames that may be in flight before the server waits for
    /// acknowledgements. `1` gives the lowest latency; `2` gives smoother
    /// motion on links with jitter.
    ///
    /// With `max_in_flight_auto` left on this is the *floor* rather than the
    /// ceiling: the session may allow itself more when the round trip it
    /// measures justifies more, and never allows itself less than this.
    pub max_in_flight: u32,
    /// Whether a session may raise `max_in_flight` on its own, up to 8, when
    /// the round trip it measures is long enough to justify it.
    ///
    /// On by default because the fixed default of 2 frames per round trip is
    /// 20 fps at 100 ms, and the transport this server is designed for is an
    /// SSH tunnel across a WAN. Turn it off to hold the window at exactly
    /// `max_in_flight`, which is what an operator tuning for latency rather
    /// than smoothness is asking for.
    pub max_in_flight_auto: bool,
    /// Seconds without a connected client after which the session is
    /// terminated. `0` keeps sessions forever (until logout).
    pub idle_timeout_secs: u64,
    /// PAM service name used to open the login session.
    pub pam_service: String,
    /// Runtime directory owned by root.
    pub runtime_dir: PathBuf,
    /// Path of the `lynxrdp-session` executable.
    pub session_binary: PathBuf,
    /// Directory for per-session log files (`<runtime_dir>/log` if unset).
    pub log_dir: Option<PathBuf>,
    /// DPI reported by the X server.
    pub dpi: u32,
}

impl Default for ListenConfig {
    fn default() -> Self {
        Self {
            address: IpAddr::from([127, 0, 0, 1]),
            port: lynxrdp_proto::DEFAULT_PORT,
            unix_socket: None,
        }
    }
}

impl Default for AccessConfig {
    fn default() -> Self {
        Self {
            min_uid: 1000,
            allow_users: Vec::new(),
            allow_groups: Vec::new(),
            deny_users: vec!["root".to_string()],
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            default_width: 1920,
            default_height: 1080,
            max_width: 4096,
            max_height: 2160,
            xserver: "Xvfb".to_string(),
            xserver_args: Vec::new(),
            startwm: "/etc/lynxrdp/startwm.sh".to_string(),
            max_fps: 60,
            max_in_flight: 2,
            max_in_flight_auto: true,
            idle_timeout_secs: 0,
            pam_service: "lynxrdp".to_string(),
            runtime_dir: PathBuf::from("/run/lynxrdp"),
            session_binary: PathBuf::from("/usr/bin/lynxrdp-session"),
            log_dir: None,
            dpi: 96,
        }
    }
}

impl Config {
    /// Parse from TOML text.
    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("parsing configuration")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Load from a file. A missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text)
                .with_context(|| format!("invalid configuration in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config::default();
                cfg.validate()?;
                Ok(cfg)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Check invariants. In particular the listen address must be loopback:
    /// the whole security model relies on connections arriving through an
    /// SSH tunnel that terminates on this host.
    pub fn validate(&self) -> Result<()> {
        if !self.listen.address.is_loopback() {
            bail!(
                "listen.address {} is not a loopback address; LynxRDP only accepts \
                 connections through SSH tunnels terminating on this host",
                self.listen.address
            );
        }
        if self.listen.port == 0 {
            bail!("listen.port must not be 0");
        }
        let s = &self.session;
        if s.default_width == 0 || s.default_height == 0 {
            bail!("session.default_width/height must be positive");
        }
        if s.max_width < s.default_width || s.max_height < s.default_height {
            bail!("session.max_width/height must be >= default_width/height");
        }
        if s.max_width > 16384 || s.max_height > 16384 {
            bail!("session.max_width/height must be <= 16384");
        }
        if s.max_fps == 0 || s.max_fps > 240 {
            bail!("session.max_fps must be between 1 and 240");
        }
        // 8 is also the ceiling the adaptive window raises itself to, so the
        // two numbers cannot disagree about what a session may queue.
        // `max_in_flight_auto` needs no check of its own beyond this one: it
        // is a bool, and the only thing that can be out of range about it is
        // the floor it is measured against, checked right here.
        if s.max_in_flight == 0 || s.max_in_flight > 8 {
            bail!("session.max_in_flight must be between 1 and 8");
        }
        if s.dpi < 48 || s.dpi > 480 {
            bail!("session.dpi must be between 48 and 480");
        }
        let r = &self.reporting;
        if r.enabled {
            if r.destination.trim().is_empty() {
                bail!("reporting.destination is required when reporting.enabled is true");
            }
            // Checked here rather than at first send so a typo is caught by
            // `lynxrdpd --check` instead of failing silently once a minute.
            crate::reporting::split_destination(&r.destination)
                .with_context(|| format!("reporting.destination {:?}", r.destination))?;
            if r.interval_secs < 5 {
                bail!("reporting.interval_secs must be at least 5");
            }
            if r.interval_secs > 86400 {
                bail!("reporting.interval_secs must be at most 86400 (a day)");
            }
            if let Some(name) = &r.node_name {
                if name.trim().is_empty() {
                    bail!("reporting.node_name must not be blank when set");
                }
            }
        }
        Ok(())
    }

    /// Socket address the daemon binds.
    pub fn listen_addr(&self) -> SocketAddr {
        SocketAddr::new(self.listen.address, self.listen.port)
    }

    /// Render the effective configuration as TOML (for `--dump-config`).
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config serialises")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_roundtrip() {
        let cfg = Config::default();
        cfg.validate().unwrap();
        let text = cfg.to_toml();
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn empty_file_is_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn non_loopback_rejected() {
        let err = Config::from_toml("[listen]\naddress = \"0.0.0.0\"\n").unwrap_err();
        assert!(err.to_string().contains("loopback"), "{err:#}");
        assert!(Config::from_toml("[listen]\naddress = \"::1\"\n").is_ok());
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(Config::from_toml("[listen]\nbogus = 1\n").is_err());
    }

    #[test]
    fn reporting_is_off_by_default() {
        let cfg = Config::default();
        assert!(!cfg.reporting.enabled);
        assert!(cfg.reporting.destination.is_empty());
        // A blank destination is fine as long as reporting stays off.
        cfg.validate().unwrap();
    }

    #[test]
    fn reporting_needs_a_destination_when_enabled() {
        let err = Config::from_toml("[reporting]\nenabled = true\n").unwrap_err();
        assert!(err.to_string().contains("destination"), "{err:#}");
    }

    #[test]
    fn reporting_destination_is_checked_at_load_time() {
        // The point is that --check catches a typo, rather than the daemon
        // failing quietly once per interval forever.
        let err =
            Config::from_toml("[reporting]\nenabled = true\ndestination = \"no-port-here\"\n")
                .unwrap_err();
        assert!(format!("{err:#}").contains("host:port"), "{err:#}");
    }

    #[test]
    fn reporting_interval_is_bounded() {
        let base = "[reporting]\nenabled = true\ndestination = \"m:9\"\n";
        assert!(Config::from_toml(&format!("{base}interval_secs = 4\n")).is_err());
        assert!(Config::from_toml(&format!("{base}interval_secs = 5\n")).is_ok());
        assert!(Config::from_toml(&format!("{base}interval_secs = 86400\n")).is_ok());
        assert!(Config::from_toml(&format!("{base}interval_secs = 86401\n")).is_err());
    }

    #[test]
    fn reporting_roundtrips_through_toml() {
        let mut cfg = Config::default();
        cfg.reporting.enabled = true;
        cfg.reporting.destination = "monitor.example.org:9999".into();
        cfg.reporting.interval_secs = 30;
        cfg.reporting.node_name = Some("desk01".into());
        cfg.validate().unwrap();
        assert_eq!(Config::from_toml(&cfg.to_toml()).unwrap(), cfg);
    }

    #[test]
    fn bounds_checked() {
        assert!(Config::from_toml("[session]\nmax_fps = 0\n").is_err());
        assert!(Config::from_toml("[session]\nmax_in_flight = 9\n").is_err());
        assert!(Config::from_toml("[session]\ndefault_width = 5000\n").is_err());
        assert!(Config::from_toml("[session]\nmax_width = 20000\n").is_err());
    }

    /// The adaptive window is on unless an operator says otherwise, and
    /// turning it off must not disturb the floor it is measured against.
    #[test]
    fn the_in_flight_window_adapts_by_default() {
        assert!(Config::default().session.max_in_flight_auto);
        let off = Config::from_toml("[session]\nmax_in_flight_auto = false\n").unwrap();
        assert!(!off.session.max_in_flight_auto);
        assert_eq!(
            off.session.max_in_flight,
            SessionConfig::default().max_in_flight
        );
    }

    #[test]
    fn missing_file_gives_defaults() {
        let cfg = Config::load(Path::new("/nonexistent/lynxrdp.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn partial_override() {
        let cfg = Config::from_toml("[access]\nmin_uid = 500\nallow_users = [\"bob\"]\n").unwrap();
        assert_eq!(cfg.access.min_uid, 500);
        assert_eq!(cfg.access.allow_users, vec!["bob".to_string()]);
        assert_eq!(cfg.listen, ListenConfig::default());
    }
}
