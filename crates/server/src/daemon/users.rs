//! Account lookups.

use std::ffi::{CStr, CString};

use anyhow::{anyhow, Result};

/// A local user account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInfo {
    /// User id.
    pub uid: u32,
    /// Primary group id.
    pub gid: u32,
    /// Login name.
    pub name: String,
    /// Home directory.
    pub home: String,
    /// Login shell.
    pub shell: String,
}

/// Look up a user by uid.
pub fn user_by_uid(uid: u32) -> Result<UserInfo> {
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: getpwuid_r writes into buffers we own; strings are copied out
    // before the buffers are dropped.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            uid,
            &mut pw,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        );
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc))
                .map_err(|e| anyhow!("getpwuid_r({uid}): {e}"));
        }
        if result.is_null() {
            return Err(anyhow!("no account with uid {uid}"));
        }
        Ok(UserInfo {
            uid: pw.pw_uid,
            gid: pw.pw_gid,
            name: CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned(),
            home: CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned(),
            shell: CStr::from_ptr(pw.pw_shell).to_string_lossy().into_owned(),
        })
    }
}

/// Names of all groups the user belongs to (primary group included).
pub fn groups_of(user: &UserInfo) -> Vec<String> {
    let name = match CString::new(user.name.as_str()) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };
    let mut gids: Vec<libc::gid_t> = vec![0; 64];
    let mut n = gids.len() as libc::c_int;
    // SAFETY: getgrouplist writes at most n gids into our buffer.
    let rc = unsafe { libc::getgrouplist(name.as_ptr(), user.gid, gids.as_mut_ptr(), &mut n) };
    if rc < 0 {
        // Buffer too small: retry with the reported size.
        gids = vec![0; (n.max(1)) as usize];
        // SAFETY: as above.
        let rc = unsafe { libc::getgrouplist(name.as_ptr(), user.gid, gids.as_mut_ptr(), &mut n) };
        if rc < 0 {
            return Vec::new();
        }
    }
    gids.truncate(n.max(0) as usize);
    gids.into_iter().filter_map(group_name).collect()
}

/// Name of a group id.
pub fn group_name(gid: u32) -> Option<String> {
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: as for getpwuid_r.
    unsafe {
        let mut gr: libc::group = std::mem::zeroed();
        let mut result: *mut libc::group = std::ptr::null_mut();
        let rc = libc::getgrgid_r(
            gid,
            &mut gr,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        );
        if rc != 0 || result.is_null() {
            return None;
        }
        Some(CStr::from_ptr(gr.gr_name).to_string_lossy().into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_exists() {
        let root = user_by_uid(0).unwrap();
        assert_eq!(root.name, "root");
        assert_eq!(root.gid, 0);
        assert!(!root.home.is_empty());
        let groups = groups_of(&root);
        assert!(groups.contains(&"root".to_string()), "{groups:?}");
    }

    #[test]
    fn unknown_uid_is_error() {
        assert!(user_by_uid(4_000_000_000).is_err());
    }
}
