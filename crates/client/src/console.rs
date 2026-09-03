//! Reattaching to the terminal that started us, on Windows.
//!
//! The client is built for the Windows GUI subsystem so that opening it from
//! Explorer does not flash up a console window behind the launcher. The cost
//! is that a GUI-subsystem process starts with no standard handles at all, so
//! `lynxrdp --help` typed at a prompt would print into nothing.
//!
//! [`attach_to_parent`] buys the console back: if we were started from one, we
//! join it and point the standard handles at it, and the command line output
//! appears where the user typed the command. If we were started from Explorer
//! there is no parent console, nothing happens, and no window appears.
//!
//! One visible difference remains, and it is inherent to the subsystem rather
//! than to this code: `cmd.exe` does not wait for a GUI-subsystem process, so
//! it returns to the prompt while the output is still arriving.

/// Attach to the parent process's console, if it has one.
///
/// Call this before anything writes to stdout or stderr -- including the
/// logger -- because the handles it installs are read at the first write.
#[cfg(windows)]
pub fn attach_to_parent() {
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    // SAFETY: all four calls are plain FFI with no borrowed memory. A failure
    // is expected whenever there is no parent console -- the Explorer case --
    // and leaves the process exactly as it was.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        // "CONOUT$" and "CONIN$" are the console's own device names: opening
        // them yields handles to the console we just attached to, whatever
        // the parent had redirected.
        let out = open(&wide("CONOUT$"));
        if out != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, out);
            SetStdHandle(STD_ERROR_HANDLE, out);
        }
        let input = open(&wide("CONIN$"));
        if input != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_INPUT_HANDLE, input);
        }

        /// UTF-16, NUL-terminated, as the wide Win32 entry points want.
        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        unsafe fn open(name: &[u16]) -> windows_sys::Win32::Foundation::HANDLE {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        }
    }
}

/// Nothing to do: every other platform starts with usable standard handles.
#[cfg(not(windows))]
pub fn attach_to_parent() {}
