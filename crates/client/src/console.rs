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
//! there is no parent console, nothing happens, and no window appears. A
//! handle a parent set on purpose is left alone: the launcher points each
//! session's stderr at a file it tails for the connection list, and that has
//! to stay where the launcher put it whatever terminal the launcher came from.
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

    use windows_sys::Win32::Foundation::{
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileType, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_UNKNOWN,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    // SAFETY: every call is plain FFI with no borrowed memory. A failure is
    // expected whenever there is no parent console -- the Explorer case --
    // and leaves the process exactly as it was.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        // Only a handle that is missing is replaced. A GUI-subsystem process
        // started from a terminal has none, which is the case this exists
        // for. A session the launcher started has all three, set on purpose:
        // stderr is the file the launcher tails for the connection list, and
        // pointing it at the terminal the launcher was typed into sent every
        // "Permission denied" there and left the list showing an exit code
        // with no detail.
        //
        // "CONOUT$" and "CONIN$" are the console's own device names: opening
        // them yields handles to the console we just attached to, whatever
        // the parent had redirected.
        let want_output = missing(STD_OUTPUT_HANDLE);
        let want_error = missing(STD_ERROR_HANDLE);
        if want_output || want_error {
            let out = open(&wide("CONOUT$"));
            if out != INVALID_HANDLE_VALUE {
                if want_output {
                    SetStdHandle(STD_OUTPUT_HANDLE, out);
                }
                if want_error {
                    SetStdHandle(STD_ERROR_HANDLE, out);
                }
            }
        }
        if missing(STD_INPUT_HANDLE) {
            let input = open(&wide("CONIN$"));
            if input != INVALID_HANDLE_VALUE {
                SetStdHandle(STD_INPUT_HANDLE, input);
            }
        }

        /// UTF-16, NUL-terminated, as the wide Win32 entry points want.
        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// Whether nothing usable is installed as this standard handle.
        ///
        /// `GetFileType` is the third test because a handle can be present
        /// and dead: one inherited from a parent that has since closed it.
        unsafe fn missing(which: STD_HANDLE) -> bool {
            let handle: HANDLE = GetStdHandle(which);
            handle.is_null()
                || handle == INVALID_HANDLE_VALUE
                || GetFileType(handle) == FILE_TYPE_UNKNOWN
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
