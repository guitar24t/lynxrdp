//! Windows installer handoff, without elevating the client or its sessions.

use std::{io, os::windows::ffi::OsStrExt, path::Path};

use anyhow::{bail, Context, Result};
use windows_sys::Win32::{
    Foundation::ERROR_CANCELLED,
    System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    },
    UI::{
        Shell::{ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW},
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: constructed only after successful COM initialization on this thread.
        unsafe { CoUninitialize() };
    }
}

pub(super) fn run(path: &Path) -> Result<()> {
    // This runs on the updater's dedicated worker, not the UI thread. Shell
    // extensions may need COM, and the worker exits after the handoff.
    let hr = unsafe {
        CoInitializeEx(
            std::ptr::null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if hr < 0 {
        bail!("initializing the installer launcher failed (HRESULT {hr:#x})");
    }
    let _apartment = Apartment;
    launch_with(path, |request| {
        // SAFETY: the initialized request and its UTF-16 filename remain alive
        // for this synchronous call. No process handle is requested or owned.
        if unsafe { ShellExecuteExW(request) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

fn launch_with(
    path: &Path,
    launch: impl FnOnce(&mut SHELLEXECUTEINFOW) -> io::Result<()>,
) -> Result<()> {
    let mut filename: Vec<u16> = path.as_os_str().encode_wide().collect();
    if filename.is_empty() || filename.contains(&0) {
        bail!("invalid installer path");
    }
    filename.push(0);
    // SAFETY: zero means unused for all optional pointer/handle fields.
    let mut request: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    request.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    // Wait for dispatch before the worker exits; report errors in our UI.
    // FLAG_NO_UI suppresses shell error dialogs, but does not suppress UAC.
    request.fMask = SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI;
    request.lpVerb = windows_sys::core::w!("runas");
    request.lpFile = filename.as_ptr();
    request.nShow = SW_SHOWNORMAL;
    match launch(&mut request) {
        Err(error) if error.raw_os_error() == Some(ERROR_CANCELLED as i32) => {
            bail!(
                "Installation cancelled. LynxRDP is still running; you can try the update again."
            );
        }
        result => result.with_context(|| format!("starting installer {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_keeps_unicode_and_spaces_in_a_single_filename() {
        let path = Path::new("C:\\Users\\Ren\u{e9} Smith\\setup.exe");
        launch_with(path, |request| {
            let expected: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: launch_with owns these terminated buffers during the callback.
            unsafe {
                assert_eq!(
                    std::slice::from_raw_parts(request.lpFile, expected.len()),
                    expected
                );
                assert_eq!(
                    std::slice::from_raw_parts(request.lpVerb, 6),
                    &[114, 117, 110, 97, 115, 0]
                );
            }
            assert!(request.lpParameters.is_null());
            assert_eq!(request.fMask, SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI);
            assert_eq!(request.nShow, SW_SHOWNORMAL);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn cancellation_and_launch_failure_do_not_report_a_handoff() {
        let path = Path::new(r"C:\setup.exe");
        let cancelled = launch_with(path, |_| Err(io::Error::from_raw_os_error(1223))).unwrap_err();
        assert!(cancelled.to_string().contains("Installation cancelled"));
        let missing = launch_with(path, |_| Err(io::Error::from_raw_os_error(2))).unwrap_err();
        assert_eq!(
            missing.downcast_ref::<io::Error>().unwrap().raw_os_error(),
            Some(2)
        );
    }

    #[test]
    fn invalid_filename_never_launches() {
        for path in ["", "C:\\setup.exe\0unexpected.exe"] {
            assert!(launch_with(Path::new(path), |_| panic!("invalid path launched")).is_err());
        }
    }
}
