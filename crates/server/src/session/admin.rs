//! Inspect and terminate only this Unix user's session processes.
use serde::Serialize;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Serialize)]
pub struct SessionRecord {
    pub pid: u32,
    pub started: u64,
    pub session_id: u64,
    pub listen: Option<String>,
}
fn inspect(pid: u32) -> io::Result<Option<SessionRecord>> {
    let path = format!("/proc/{pid}");
    if fs::metadata(&path)?.uid() != crate::peer::own_uid() {
        return Ok(None);
    }
    let executable = fs::read_link(format!("{path}/exe"))?;
    let name = executable.file_name().unwrap_or_default().to_string_lossy();
    if name != "lynxrdp-session" && name != "lynxrdp-session (deleted)" {
        return Ok(None);
    }
    let args = fs::read(format!("{path}/cmdline"))?;
    let args: Vec<_> = args
        .split(|&b| b == 0)
        .map(String::from_utf8_lossy)
        .collect();
    let value = |key: &str| {
        args.iter().enumerate().find_map(|(index, arg)| {
            if arg.as_ref() == key {
                args.get(index + 1).map(|v| v.to_string())
            } else {
                arg.strip_prefix(&format!("{key}=")).map(str::to_string)
            }
        })
    };
    // Listing helpers are the same executable, but are not desktop sessions.
    if value("--listen").is_none() && value("--control-fd").is_none() {
        return Ok(None);
    }
    let stat = fs::read_to_string(format!("{path}/stat"))?;
    let started = start_time(&stat).ok_or_else(|| io::Error::other("invalid process stat"))?;
    Ok(Some(SessionRecord {
        pid,
        started,
        session_id: value("--session-id")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        listen: value("--listen"),
    }))
}
fn start_time(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}
/// Lists existing sessions without creating or taking over a desktop.
pub fn list() -> io::Result<Vec<SessionRecord>> {
    let mut result = Vec::new();
    for entry in fs::read_dir("/proc")?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        if let Ok(Some(record)) = inspect(pid) {
            result.push(record);
        }
    }
    result.sort_by_key(|r| r.pid);
    Ok(result)
}
/// Pin the process before revalidating identity, so PID reuse cannot kill a successor.
pub fn terminate(pid: u32, started: u64) -> io::Result<()> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::other("invalid session PID"));
    }
    // SAFETY: pidfd_open takes a positive pid and zero flags, returning a new fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh owned descriptor from pidfd_open.
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let record =
        inspect(pid)?.ok_or_else(|| io::Error::other("not one of your desktop sessions"))?;
    if record.started != started {
        return Err(io::Error::other("session changed; refresh the list"));
    }
    // SAFETY: valid pidfd, SIGTERM, no siginfo override and zero flags.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn process_names_with_parentheses_do_not_shift_start_time() {
        let stat = format!(
            "42 (name ) with space) S {} 1234 0",
            vec!["0"; 18].join(" ")
        );
        assert_eq!(start_time(&stat), Some(1234));
    }
    #[test]
    fn arbitrary_processes_cannot_be_terminated() {
        assert!(terminate(std::process::id(), 0).is_err());
        assert!(terminate(0, 0).is_err());
    }
}
