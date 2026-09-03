//! Access policy evaluation.

use crate::config::AccessConfig;

/// Decide whether `user` (with `groups`) may open a session. Returns the
/// refusal reason on denial.
pub fn check(cfg: &AccessConfig, uid: u32, user: &str, groups: &[String]) -> Result<(), String> {
    if cfg.deny_users.iter().any(|u| u == user) {
        return Err(format!("user {user} is denied by configuration"));
    }
    if uid < cfg.min_uid {
        return Err(format!(
            "uid {uid} is below access.min_uid ({})",
            cfg.min_uid
        ));
    }
    let user_ok = cfg.allow_users.is_empty() || cfg.allow_users.iter().any(|u| u == user);
    let group_ok =
        cfg.allow_groups.is_empty() || groups.iter().any(|g| cfg.allow_groups.contains(g));
    if cfg.allow_users.is_empty() && cfg.allow_groups.is_empty() {
        return Ok(());
    }
    // When both lists are set, membership in either is sufficient.
    let listed_user = !cfg.allow_users.is_empty() && user_ok;
    let listed_group = !cfg.allow_groups.is_empty() && group_ok;
    if listed_user || listed_group {
        Ok(())
    } else {
        Err(format!(
            "user {user} is not in access.allow_users or access.allow_groups"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(min_uid: u32, users: &[&str], groups: &[&str], deny: &[&str]) -> AccessConfig {
        AccessConfig {
            min_uid,
            allow_users: users.iter().map(|s| s.to_string()).collect(),
            allow_groups: groups.iter().map(|s| s.to_string()).collect(),
            deny_users: deny.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn default_policy() {
        let c = AccessConfig::default();
        assert!(check(&c, 1000, "alice", &[]).is_ok());
        assert!(check(&c, 0, "root", &[]).is_err());
        assert!(check(&c, 999, "svc", &[]).is_err());
    }

    #[test]
    fn allow_lists() {
        let c = cfg(1000, &["bob"], &[], &[]);
        assert!(check(&c, 1000, "bob", &[]).is_ok());
        assert!(check(&c, 1001, "alice", &[]).is_err());
        let c = cfg(1000, &[], &["remote"], &[]);
        assert!(check(&c, 1001, "alice", &["users".into(), "remote".into()]).is_ok());
        assert!(check(&c, 1001, "alice", &["users".into()]).is_err());
        let c = cfg(1000, &["bob"], &["remote"], &[]);
        assert!(check(&c, 1000, "bob", &[]).is_ok());
        assert!(check(&c, 1001, "alice", &["remote".into()]).is_ok());
        assert!(check(&c, 1002, "carol", &["users".into()]).is_err());
    }

    #[test]
    fn deny_wins() {
        let c = cfg(0, &["root"], &[], &["root"]);
        assert!(check(&c, 0, "root", &[]).is_err());
    }
}
