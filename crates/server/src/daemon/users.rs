//! Account lookups.

use std::ffi::{CStr, CString};

use anyhow::{anyhow, bail, Result};

/// Upper bound on a plausible group list, used to sanity-check what
/// `getgrouplist` asks us for before we allocate it.
const MAX_GROUPS: libc::c_int = 65_536;

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

/// Group ids the user belongs to, primary group included.
///
/// Separate from [`groups_of`] because the supervisor needs the numbers rather
/// than the names, and needs them *before* it forks: this resolves them through
/// NSS, which may dlopen a module, allocate, and open a socket to sssd. None of
/// that is legal between `fork` and `exec`, which is where `initgroups(3)` used
/// to do it -- in a process that had just pulled the whole name service stack
/// in through libpam, so the allocator lock it needs may well have been held by
/// another thread at the moment of the fork.
///
/// Returns an error rather than an empty list. "No supplementary groups" is not
/// a safe default for a caller about to call `setgroups`: it is the user
/// silently losing `video`, `audio`, `input` and everything else their desktop
/// needs, which surfaces as a session that half works instead of a failure.
///
/// That only covers the failures `getgrouplist` reports. It answers for an
/// account it cannot resolve at all with the one gid it was handed, and there
/// is no way to tell that apart from a user who genuinely has no supplementary
/// groups -- so a directory service that is down still degrades quietly here,
/// exactly as `initgroups` did.
pub fn group_ids(user: &str, primary_gid: u32) -> Result<Vec<u32>> {
    let name = CString::new(user).map_err(|_| anyhow!("user name {user:?} contains a NUL"))?;
    // getgrouplist reports the size it needs by rewriting `n`, so a first call
    // that fails is also the sizing call for the second.
    let mut gids: Vec<libc::gid_t> = vec![0; 64];
    let mut n = gids.len() as libc::c_int;
    // SAFETY: getgrouplist writes at most n gids into a buffer of n.
    if unsafe { libc::getgrouplist(name.as_ptr(), primary_gid, gids.as_mut_ptr(), &mut n) } < 0 {
        if n <= 0 || n > MAX_GROUPS {
            bail!("getgrouplist({user}) asked for room for {n} groups");
        }
        gids = vec![0; n as usize];
        // SAFETY: as above, now with the size it asked for.
        if unsafe { libc::getgrouplist(name.as_ptr(), primary_gid, gids.as_mut_ptr(), &mut n) } < 0
        {
            bail!("getgrouplist({user}) failed with room for {n} groups");
        }
    }
    if n < 0 {
        bail!("getgrouplist({user}) reported {n} groups");
    }
    gids.truncate(n as usize);
    // Documented to be included, but the caller is about to hand this to
    // setgroups and cannot afford to find out otherwise.
    if !gids.contains(&primary_gid) {
        gids.push(primary_gid);
    }
    Ok(gids)
}

/// Names of all groups the user belongs to (primary group included).
pub fn groups_of(user: &UserInfo) -> Vec<String> {
    match group_ids(&user.name, user.gid) {
        Ok(gids) => gids.into_iter().filter_map(group_name).collect(),
        Err(e) => {
            // Empty is the safe answer for an access check -- `allow_groups`
            // then matches nothing -- but it used to be a silent one, and a
            // user refused because the directory service was down deserves to
            // find out why from the log.
            log::warn!("could not resolve the groups of {}: {e:#}", user.name);
            Vec::new()
        }
    }
}

/// Name of a group id.
///
/// The failure worth knowing about is logged rather than swallowed: this feeds
/// the `allow_groups` check, and a lookup that fails is a member of exactly
/// the group the administrator granted access with being refused, with `id`
/// showing them in it and nothing pointing at the cause.
pub fn group_name(gid: u32) -> Option<String> {
    // SAFETY: a zeroed `group` is a valid value for getgrgid_r to fill in.
    let mut gr: libc::group = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: getgrgid_r writes into buffers we own, and `_buf`, which the
    // strings in `gr` point into, lives until they have been copied out.
    let (rc, _buf) = grow_until_fits(|buf| unsafe {
        libc::getgrgid_r(
            gid,
            &mut gr,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    });
    if rc != 0 {
        log::warn!(
            "cannot resolve gid {gid} to a name: {}",
            std::io::Error::from_raw_os_error(rc)
        );
        return None;
    }
    if result.is_null() {
        return None;
    }
    // SAFETY: a non-null result means gr_name points into `_buf`, still alive.
    Some(
        unsafe { CStr::from_ptr(gr.gr_name) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// First buffer offered to a `*_r` lookup.
const LOOKUP_BUF_INITIAL: usize = 16 * 1024;

/// Largest buffer a `*_r` lookup is retried with.
///
/// A group entry carries its whole member list, so this has to hold the
/// biggest group a directory service will serve: a megabyte is a hundred
/// thousand short names, past any group `allow_groups` is going to name.
const LOOKUP_BUF_MAX: usize = 1024 * 1024;

/// Call a reentrant NSS lookup with a buffer that grows until the entry fits.
///
/// `getgrgid_r` copies the group's entire membership into the caller's
/// buffer and answers `ERANGE` when it does not fit. A single fixed 16 KiB
/// call held about nine hundred members, which is smaller than any
/// organisation-wide group -- and its `ERANGE` was indistinguishable from
/// "no such group", so every member of the one group that mattered was
/// quietly refused. The buffer comes back with the result because the
/// strings in the caller's struct point into it.
fn grow_until_fits(mut lookup: impl FnMut(&mut [u8]) -> libc::c_int) -> (libc::c_int, Vec<u8>) {
    let mut buf = vec![0u8; LOOKUP_BUF_INITIAL];
    loop {
        let rc = lookup(&mut buf);
        if rc != libc::ERANGE || buf.len() >= LOOKUP_BUF_MAX {
            return (rc, buf);
        }
        let grown = (buf.len() * 2).min(LOOKUP_BUF_MAX);
        buf = vec![0u8; grown];
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
    fn group_ids_always_include_the_primary_group() {
        let root = user_by_uid(0).unwrap();
        let gids = group_ids(&root.name, root.gid).unwrap();
        assert!(gids.contains(&root.gid), "{gids:?}");
        // A gid nobody is in is still returned: setgroups needs it there.
        let gids = group_ids(&root.name, 4_000_000_001).unwrap();
        assert!(gids.contains(&4_000_000_001), "{gids:?}");
    }

    #[test]
    fn a_name_with_a_nul_is_an_error_not_an_empty_list() {
        assert!(group_ids("ro\0ot", 0).is_err());
    }

    #[test]
    fn unknown_uid_is_error() {
        assert!(user_by_uid(4_000_000_000).is_err());
    }

    /// A lookup that keeps asking for room is given more, and gets its answer
    /// with the buffer that finally fitted -- not the 16 KiB that used to be
    /// its only chance.
    #[test]
    fn a_lookup_grows_its_buffer_until_the_entry_fits() {
        let needed = 200 * 1024;
        let mut calls = 0;
        let (rc, buf) = grow_until_fits(|buf| {
            calls += 1;
            if buf.len() < needed {
                libc::ERANGE
            } else {
                0
            }
        });
        assert_eq!(rc, 0);
        assert!(buf.len() >= needed, "{}", buf.len());
        assert!(buf.len() <= LOOKUP_BUF_MAX);
        // Doubling from 16 KiB: 16, 32, 64, 128, 256.
        assert_eq!(calls, 5);
    }

    /// An entry that never fits stops at the cap with ERANGE rather than
    /// growing forever, and a lookup that fails for any other reason is not
    /// retried at all.
    #[test]
    fn a_lookup_that_never_fits_stops_at_the_cap() {
        let mut calls = 0;
        let (rc, buf) = grow_until_fits(|_| {
            calls += 1;
            libc::ERANGE
        });
        assert_eq!(rc, libc::ERANGE);
        assert_eq!(buf.len(), LOOKUP_BUF_MAX);
        assert!(calls < 20, "{calls}");

        let mut calls = 0;
        let (rc, buf) = grow_until_fits(|_| {
            calls += 1;
            libc::EIO
        });
        assert_eq!(rc, libc::EIO);
        assert_eq!(buf.len(), LOOKUP_BUF_INITIAL);
        assert_eq!(calls, 1);
    }
}
