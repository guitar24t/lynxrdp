//! Opening PAM login sessions without linking against libpam at build time.
//!
//! `libpam.so.0` is loaded at runtime so the same binary works on systems
//! with and without PAM development files, and degrades gracefully when the
//! library is absent. Only the session half of PAM is used: authentication
//! has already happened over SSH.

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::ptr;

use anyhow::{anyhow, bail, Result};
use libloading::{Library, Symbol};

const PAM_SUCCESS: c_int = 0;
const PAM_CONV_ERR: c_int = 19;
const PAM_ESTABLISH_CRED: c_int = 0x0002;
const PAM_DELETE_CRED: c_int = 0x0004;
const PAM_TTY: c_int = 3;
const PAM_RHOST: c_int = 4;
const PAM_XDISPLAY: c_int = 11;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(
    c_int,
    *mut *const PamMessage,
    *mut *mut PamResponse,
    *mut c_void,
) -> c_int;

#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

/// We never authenticate, so any prompt is an error.
unsafe extern "C" fn no_conversation(
    _n: c_int,
    _msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    _data: *mut c_void,
) -> c_int {
    if !resp.is_null() {
        *resp = ptr::null_mut();
    }
    PAM_CONV_ERR
}

type PamStart =
    unsafe extern "C" fn(*const c_char, *const c_char, *const PamConv, *mut *mut c_void) -> c_int;
type PamEnd = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type PamInt = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type PamPutenv = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;
type PamGetenvlist = unsafe extern "C" fn(*mut c_void) -> *mut *mut c_char;
type PamSetItem = unsafe extern "C" fn(*mut c_void, c_int, *const c_void) -> c_int;
type PamStrerror = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;

/// The loaded PAM library.
pub struct Pam {
    lib: Library,
}

impl Pam {
    /// Load `libpam.so.0`.
    pub fn load() -> Result<Self> {
        // SAFETY: loading the system PAM library; it has no unsafe init side effects.
        let lib = unsafe { Library::new("libpam.so.0") }
            .map_err(|e| anyhow!("cannot load libpam.so.0: {e}"))?;
        let pam = Self { lib };
        // Verify the symbols we need exist.
        pam.sym::<PamStart>(b"pam_start\0")?;
        pam.sym::<PamEnd>(b"pam_end\0")?;
        pam.sym::<PamInt>(b"pam_open_session\0")?;
        pam.sym::<PamInt>(b"pam_close_session\0")?;
        pam.sym::<PamGetenvlist>(b"pam_getenvlist\0")?;
        Ok(pam)
    }

    fn sym<T>(&self, name: &[u8]) -> Result<Symbol<'_, T>> {
        // SAFETY: the caller supplies the correct function type for the symbol.
        unsafe { self.lib.get(name) }
            .map_err(|e| anyhow!("missing PAM symbol {}: {e}", String::from_utf8_lossy(name)))
    }

    /// Open a login session for `user` using PAM service `service`.
    ///
    /// Runs account management, establishes credentials and opens the
    /// session. `env` entries are exported to the PAM environment before
    /// the session is opened (so `pam_systemd` sees e.g. `XDG_SESSION_TYPE`).
    pub fn open_session(
        &self,
        service: &str,
        user: &str,
        env: &[(&str, &str)],
    ) -> Result<PamSession<'_>> {
        let c_service = CString::new(service)?;
        let c_user = CString::new(user)?;
        let conv = PamConv {
            conv: Some(no_conversation),
            appdata_ptr: ptr::null_mut(),
        };
        let mut handle: *mut c_void = ptr::null_mut();
        // SAFETY: all pointers are valid for the duration of the calls; the
        // handle is owned by the returned PamSession.
        unsafe {
            let start = self.sym::<PamStart>(b"pam_start\0")?;
            let rc = start(c_service.as_ptr(), c_user.as_ptr(), &conv, &mut handle);
            if rc != PAM_SUCCESS || handle.is_null() {
                bail!(
                    "pam_start({service}, {user}) failed: {}",
                    self.strerror(ptr::null_mut(), rc)
                );
            }
            let mut session = PamSession {
                pam: self,
                handle,
                opened: false,
                creds: false,
            };
            let set_item = self.sym::<PamSetItem>(b"pam_set_item\0")?;
            let tty = CString::new("lynxrdp")?;
            set_item(handle, PAM_TTY, tty.as_ptr() as *const c_void);
            let rhost = CString::new("localhost")?;
            set_item(handle, PAM_RHOST, rhost.as_ptr() as *const c_void);
            let xdisplay = CString::new("lynxrdp")?;
            set_item(handle, PAM_XDISPLAY, xdisplay.as_ptr() as *const c_void);
            let putenv = self.sym::<PamPutenv>(b"pam_putenv\0")?;
            for (k, v) in env {
                let kv = CString::new(format!("{k}={v}"))?;
                let rc = putenv(handle, kv.as_ptr());
                if rc != PAM_SUCCESS {
                    log::warn!("pam_putenv({k}) failed: {}", self.strerror(handle, rc));
                }
            }
            let acct = self.sym::<PamInt>(b"pam_acct_mgmt\0")?;
            let rc = acct(handle, 0);
            if rc != PAM_SUCCESS {
                let msg = self.strerror(handle, rc);
                drop(session);
                bail!("account check for {user} failed: {msg}");
            }
            let setcred = self.sym::<PamInt>(b"pam_setcred\0")?;
            let rc = setcred(handle, PAM_ESTABLISH_CRED);
            if rc != PAM_SUCCESS {
                log::warn!("pam_setcred failed: {}", self.strerror(handle, rc));
            } else {
                session.creds = true;
            }
            let open = self.sym::<PamInt>(b"pam_open_session\0")?;
            let rc = open(handle, 0);
            if rc != PAM_SUCCESS {
                let msg = self.strerror(handle, rc);
                drop(session);
                bail!("pam_open_session for {user} failed: {msg}");
            }
            session.opened = true;
            Ok(session)
        }
    }

    unsafe fn strerror(&self, handle: *mut c_void, rc: c_int) -> String {
        match self.sym::<PamStrerror>(b"pam_strerror\0") {
            Ok(f) => {
                let p = f(handle, rc);
                if p.is_null() {
                    format!("error {rc}")
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            }
            Err(_) => format!("error {rc}"),
        }
    }
}

/// An open PAM session; closed on drop.
pub struct PamSession<'a> {
    pam: &'a Pam,
    handle: *mut c_void,
    opened: bool,
    creds: bool,
}

impl PamSession<'_> {
    /// Environment variables exported by PAM modules (e.g. `XDG_RUNTIME_DIR`).
    pub fn env(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        // SAFETY: pam_getenvlist returns a NULL-terminated array we must free.
        unsafe {
            let Ok(getenvlist) = self.pam.sym::<PamGetenvlist>(b"pam_getenvlist\0") else {
                return out;
            };
            let list = getenvlist(self.handle);
            if list.is_null() {
                return out;
            }
            let mut i = 0;
            loop {
                let p = *list.add(i);
                if p.is_null() {
                    break;
                }
                let s = CStr::from_ptr(p).to_string_lossy().into_owned();
                if let Some((k, v)) = s.split_once('=') {
                    out.push((k.to_string(), v.to_string()));
                }
                libc::free(p as *mut c_void);
                i += 1;
            }
            libc::free(list as *mut c_void);
        }
        out
    }

    /// Close the session explicitly.
    pub fn close(&mut self) {
        // SAFETY: handle is valid until pam_end.
        unsafe {
            if self.opened {
                if let Ok(f) = self.pam.sym::<PamInt>(b"pam_close_session\0") {
                    let rc = f(self.handle, 0);
                    if rc != PAM_SUCCESS {
                        log::warn!(
                            "pam_close_session failed: {}",
                            self.pam.strerror(self.handle, rc)
                        );
                    }
                }
                self.opened = false;
            }
            if self.creds {
                if let Ok(f) = self.pam.sym::<PamInt>(b"pam_setcred\0") {
                    f(self.handle, PAM_DELETE_CRED);
                }
                self.creds = false;
            }
            if !self.handle.is_null() {
                if let Ok(f) = self.pam.sym::<PamEnd>(b"pam_end\0") {
                    f(self.handle, PAM_SUCCESS);
                }
                self.handle = ptr::null_mut();
            }
        }
    }
}

impl Drop for PamSession<'_> {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_is_graceful() {
        // Either PAM loads or reports a clean error; never panics.
        match Pam::load() {
            Ok(_) => {}
            Err(e) => assert!(e.to_string().contains("libpam"), "{e}"),
        }
    }

    #[test]
    fn unknown_service_fails_cleanly() {
        let Ok(pam) = Pam::load() else { return };
        // Opening a session for a bogus service must not succeed silently
        // unless PAM's "other" fallback permits it; either way no panic.
        let _ = pam.open_session("lynxrdp-nonexistent-test-service", "nobody", &[]);
    }
}
