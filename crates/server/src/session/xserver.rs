//! Launching the headless X server (Xvfb by default).

use std::fs;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::xauth;

/// Settings for the X server process.
#[derive(Clone, Debug)]
pub struct XServerConfig {
    /// Executable (`Xvfb`).
    pub program: String,
    /// Additional arguments appended to the generated ones.
    pub extra_args: Vec<String>,
    /// Virtual screen width (the largest size a client may ask for).
    pub max_width: u32,
    /// Virtual screen height.
    pub max_height: u32,
    /// DPI to report.
    pub dpi: u32,
    /// Directory for the authority file and other private files.
    pub runtime_dir: PathBuf,
}

/// A running X server.
pub struct XServer {
    child: Child,
    display_num: u32,
    xauth_path: PathBuf,
}

impl XServer {
    /// Start the server and wait until it reports its display number.
    pub fn spawn(cfg: &XServerConfig) -> Result<Self> {
        ensure_private_dir(&cfg.runtime_dir)?;
        let cookie = xauth::random_cookie()?;
        let xauth_path = cfg
            .runtime_dir
            .join(format!("Xauthority-{}", std::process::id()));
        // Placeholder display number; rewritten once known. The X server only
        // reads the cookie from the file, not the display number.
        xauth::write_file(&xauth_path, 0, &cookie)?;

        let mut fds = [0i32; 2];
        // SAFETY: fds is a valid 2-element array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe");
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // SAFETY: we own read_fd.
        let mut reader = unsafe { fs::File::from_raw_fd(read_fd) };

        let mut cmd = Command::new(&cfg.program);
        cmd.arg("-displayfd")
            .arg(write_fd.to_string())
            .arg("-screen")
            .arg("0")
            .arg(format!("{}x{}x24", cfg.max_width, cfg.max_height))
            .arg("-nolisten")
            .arg("tcp")
            .arg("-noreset")
            .arg("-auth")
            .arg(&xauth_path)
            .arg("-dpi")
            .arg(cfg.dpi.to_string())
            .args([
                "+extension",
                "RANDR",
                "+extension",
                "DAMAGE",
                "+extension",
                "XFIXES",
                "+extension",
                "MIT-SHM",
                "+extension",
                "XTEST",
            ])
            .args(&cfg.extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .env_remove("DISPLAY");
        // SAFETY: only async-signal-safe calls (close, setsid) in the child.
        unsafe {
            cmd.pre_exec(move || {
                libc::close(read_fd);
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting X server {}", cfg.program))?;
        // SAFETY: we own write_fd in the parent; the child has its own copy.
        unsafe {
            libc::close(write_fd);
        }

        // Xvfb writes "<n>\n" to displayfd once it is listening.
        let display_num =
            match read_display_number(&mut reader, &mut child, Duration::from_secs(20)) {
                Ok(n) => n,
                Err(e) => {
                    let _ = child.kill();
                    let mut err = String::new();
                    if let Some(mut se) = child.stderr.take() {
                        let _ = se.read_to_string(&mut err);
                    }
                    let _ = child.wait();
                    let _ = fs::remove_file(&xauth_path);
                    return Err(e.context(format!("X server failed to start: {}", err.trim())));
                }
            };
        xauth::write_file(&xauth_path, display_num, &cookie)?;
        // Drain stderr on a helper thread so the X server never blocks on it.
        if let Some(se) = child.stderr.take() {
            std::thread::Builder::new()
                .name("xserver-stderr".into())
                .spawn(move || {
                    let mut se = se;
                    let mut buf = String::new();
                    let _ = se.read_to_string(&mut buf);
                    for line in buf.lines().filter(|l| !l.trim().is_empty()) {
                        log::debug!("Xserver: {line}");
                    }
                })
                .ok();
        }
        log::info!("X server pid {} on display :{display_num}", child.id());
        Ok(Self {
            child,
            display_num,
            xauth_path,
        })
    }

    /// `:N` display string.
    pub fn display(&self) -> String {
        format!(":{}", self.display_num)
    }

    /// Display number.
    pub fn display_num(&self) -> u32 {
        self.display_num
    }

    /// Authority file path (export as `XAUTHORITY`).
    pub fn xauth_path(&self) -> &Path {
        &self.xauth_path
    }

    /// Process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Whether the server process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate the server.
    pub fn shutdown(&mut self) {
        // SAFETY: sending a signal to our own child.
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.xauth_path);
    }
}

impl Drop for XServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn read_display_number(reader: &mut fs::File, child: &mut Child, timeout: Duration) -> Result<u32> {
    // Poll with a timeout so a hung server does not block us forever.
    let deadline = Instant::now() + timeout;
    let mut text = String::new();
    let mut buf = [0u8; 16];
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            bail!("X server exited during startup with {status}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timed out waiting for the X server to start");
        }
        let mut pfd = libc::pollfd {
            fd: std::os::unix::io::AsRawFd::as_raw_fd(reader),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pfd is valid for the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis().min(200) as i32) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("poll displayfd");
        }
        if rc == 0 {
            continue;
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            bail!("X server closed displayfd without reporting a display");
        }
        text.push_str(&String::from_utf8_lossy(&buf[..n]));
        if let Some(line) = text.lines().next() {
            if text.contains('\n') {
                return line
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| anyhow!("bad displayfd output {line:?}"));
            }
        }
    }
}

/// Create `dir` with mode 0700 if missing and verify it is a private
/// directory owned by us (guards against symlink tricks in shared /tmp).
pub fn ensure_private_dir(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                bail!("{} exists and is not a directory", dir.display());
            }
            if meta.uid() != crate::peer::own_uid() {
                bail!("{} is not owned by us", dir.display());
            }
            if meta.permissions().mode() & 0o077 != 0 {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("securing {}", dir.display()))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dir.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::DirBuilder::new()
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("creating {}", dir.display()))
        }
        Err(e) => Err(e).with_context(|| format!("inspecting {}", dir.display())),
    }
}

/// Per-user private runtime directory for session files.
pub fn default_runtime_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(xdg);
        if p.is_dir() {
            return p.join("lynxrdp");
        }
    }
    std::env::temp_dir().join(format!("lynxrdp-{}", crate::peer::own_uid()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_dir_is_created_and_checked() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("rt");
        ensure_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Loosened permissions are tightened again.
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A file in the way is an error.
        let f = tmp.path().join("file");
        fs::write(&f, b"x").unwrap();
        assert!(ensure_private_dir(&f).is_err());
    }

    #[test]
    #[ignore = "needs Xvfb"]
    fn spawns_xvfb() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = XServerConfig {
            program: "Xvfb".into(),
            extra_args: vec![],
            max_width: 640,
            max_height: 480,
            dpi: 96,
            runtime_dir: tmp.path().join("rt"),
        };
        let mut xs = XServer::spawn(&cfg).unwrap();
        assert!(xs.is_running());
        assert!(xs.xauth_path().exists());
        let d = xs.display();
        assert!(d.starts_with(':'));
        xs.shutdown();
        assert!(!xs.xauth_path().exists());
    }
}
