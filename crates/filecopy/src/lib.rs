//! Native metadata-only file offers; contents are requested only on use.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{Fetch, Files};
#[cfg(any(not(target_os = "linux"), test))]
mod source;
#[cfg(not(target_os = "linux"))]
pub use source::Fetch;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Files;
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(test, allow(dead_code))]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Files;

#[cfg(any(target_os = "macos", test))]
mod dav;
