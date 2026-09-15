//! Bounded, memory-only diagnostics for SSH launched without a terminal.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::process::ChildStderr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const TAIL_BYTES: usize = 8192;

pub(super) struct Capture {
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<VecDeque<u8>>>,
}

impl Capture {
    pub(super) fn start(mut pipe: ChildStderr) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = pipe.as_raw_fd();
            // We own the read end. Nonblocking reads let shutdown finish even
            // if a ProxyCommand descendant inherits stderr and outlives SSH.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error());
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let reader = thread::Builder::new()
            .name("ssh-stderr".into())
            .spawn(move || {
                let mut tail = VecDeque::with_capacity(TAIL_BYTES);
                let mut buf = [0; 4096];
                while !stopping.load(Ordering::Acquire) {
                    match read_available(&mut pipe, &mut buf) {
                        Ok(0) => return tail,
                        Ok(n) => append(&mut tail, &buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::park_timeout(Duration::from_millis(10));
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => return tail,
                    }
                }
                // SSH has exited or been killed. Collect its final pipe buffer
                // before returning, but bound this too: a surviving descendant
                // could otherwise keep writing forever during shutdown.
                for _ in 0..64 {
                    match read_available(&mut pipe, &mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => append(&mut tail, &buf[..n]),
                    }
                }
                tail
            })?;
        Ok(Self {
            stop,
            reader: Some(reader),
        })
    }

    pub(super) fn finish(&mut self) -> String {
        self.stop.store(true, Ordering::Release);
        let Some(reader) = self.reader.take() else {
            return String::new();
        };
        reader.thread().unpark();
        let bytes: Vec<_> = reader.join().unwrap_or_default().into_iter().collect();
        // SSH banners can contain arbitrary bytes; keep readable diagnostics,
        // not terminal controls, in the GUI. Lossy decoding also handles a
        // multibyte character split at the bounded tail's first byte.
        String::from_utf8_lossy(&bytes)
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            .collect::<String>()
            .trim()
            .to_owned()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.finish();
    }
}

fn append(tail: &mut VecDeque<u8>, bytes: &[u8]) {
    let discard = (tail.len() + bytes.len()).saturating_sub(TAIL_BYTES);
    tail.drain(..discard.min(tail.len()));
    tail.extend(&bytes[bytes.len().saturating_sub(TAIL_BYTES)..]);
}

fn read_available(pipe: &mut ChildStderr, buf: &mut [u8]) -> io::Result<usize> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE;
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        let mut available = 0;
        // Only this thread reads this handle. Peek prevents a blocking read
        // when there is no data, including when a descendant holds it open.
        let ok = unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            return if err.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                Ok(0)
            } else {
                Err(err)
            };
        }
        if available == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let len = buf.len().min(available as usize);
        pipe.read(&mut buf[..len])
    }
    #[cfg(not(windows))]
    pipe.read(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    #[test]
    fn native_pipe_drains_verbose_output_and_retains_the_last_error() {
        // Exercise the Windows pipe implementation on Windows CI as well as
        // fcntl on Unix; Tunnel's shell fixtures only run on Unix.
        #[cfg(windows)]
        let mut command = {
            let mut cmd = Command::new("cmd");
            cmd.args(["/d", "/c", "(echo discard-prefix & for /L %i in (1,1,4096) do @echo verbose-debug-line) 1>&2 & echo Permission denied 1>&2"]);
            cmd
        };
        #[cfg(unix)]
        let mut command = {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "printf 'discard-prefix\\n' >&2; i=0; while [ $i -lt 4096 ]; do echo verbose-debug-line >&2; i=$((i+1)); done; printf 'Permission denied\\n' >&2"]);
            cmd
        };
        let mut child = command
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut capture = Capture::start(child.stderr.take().unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("SSH-like output blocked on an undrained pipe");
            }
            thread::sleep(Duration::from_millis(10));
        }
        let text = capture.finish();
        assert!(text.ends_with("Permission denied"), "{text}");
        assert!(!text.contains("discard-prefix"));
        assert!(text.len() < 10_000);
    }
}
