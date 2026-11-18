//! Where things live. Normal run: data in `%LOCALAPPDATA%\Local Tugboat`, library in
//! `%USERPROFILE%\Local Tugboat` (not under Pictures: OneDrive folder backup would upload it).
//!
//! QA runs set `TUGBOAT_PROFILE=<absolute dir>`: data, Bin and the default library all go under
//! that dir and the Credential Manager entry gets its own name, so a test run can never touch
//! the real library or the real identity key.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub bin_dir: PathBuf,
    pub default_library: PathBuf,
    /// Credential Manager user name for the identity key.
    pub key_user: String,
}

pub const PROFILE_ENV: &str = "TUGBOAT_PROFILE";
pub const BIND_ENV: &str = "TUGBOAT_BIND";

/// Resolve from an environment lookup (a parameter so tests need no real environment).
pub fn resolve(env: &dyn Fn(&str) -> Option<String>) -> Result<Paths, String> {
    if let Some(profile) = env(PROFILE_ENV).filter(|p| !p.trim().is_empty()) {
        let dir = PathBuf::from(&profile);
        if !dir.is_absolute() {
            return Err(format!("{PROFILE_ENV} must be a full path, got '{profile}'"));
        }
        // Different profile dirs get different key entries.
        let tag = blake3::hash(profile.to_lowercase().as_bytes()).to_hex().to_string();
        return Ok(Paths {
            bin_dir: dir.join("Bin"),
            default_library: dir.join("library"),
            key_user: format!("pc-secret-key.profile-{}", &tag[..12]),
            data_dir: dir,
        });
    }
    let local = env("LOCALAPPDATA").ok_or("LOCALAPPDATA is not set")?;
    let home = env("USERPROFILE").ok_or("USERPROFILE is not set")?;
    let data_dir = PathBuf::from(local).join("Local Tugboat");
    Ok(Paths { bin_dir: data_dir.join("Bin"), default_library: PathBuf::from(home).join("Local Tugboat"), key_user: "pc-secret-key".into(), data_dir })
}

pub fn resolve_from_env() -> Result<Paths, String> {
    resolve(&|k| std::env::var(k).ok())
}

/// UDP bind address from `TUGBOAT_BIND` (QA: "127.0.0.1"): an IP alone uses `port`, "ip:port"
/// is taken as is. Unset = every interface (the real run).
pub fn parse_bind(value: Option<&str>, port: u16) -> Result<std::net::SocketAddr, String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(std::net::SocketAddr::from(([0, 0, 0, 0], port))),
        Some(v) => v
            .parse::<std::net::SocketAddr>()
            .or_else(|_| v.parse::<std::net::IpAddr>().map(|ip| std::net::SocketAddr::new(ip, port)))
            .map_err(|_| format!("{BIND_ENV} must be an IP address or ip:port, got '{v}'")),
    }
}

/// QA only: `TUGBOAT_FIREWALL=skip` turns the firewall check off so a screenshot run on a PC
/// with its own firewall rules is not covered by a banner. Ignored without TUGBOAT_PROFILE: a
/// real run can never hide a real blocked-by-firewall problem.
pub const FIREWALL_ENV: &str = "TUGBOAT_FIREWALL";

pub fn firewall_check_allowed(profile_set: bool, value: Option<&str>) -> bool {
    !(profile_set && value.is_some_and(|v| v.trim().eq_ignore_ascii_case("skip")))
}

/// mDNS binds multicast sockets on every interface, which pops the Windows Firewall dialog. A run
/// that pinned the bind address (TUGBOAT_BIND, QA) must stay on loopback, and a loopback
/// endpoint cannot be discovered anyway: no mDNS then.
pub fn mdns_allowed(bind_env: Option<&str>) -> bool {
    bind_env.is_none_or(|v| v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn normal_run_uses_local_app_data_and_the_home_folder_outside_pictures() {
        let p = resolve(&env(&[("LOCALAPPDATA", r"C:\Users\Sam\AppData\Local"), ("USERPROFILE", r"C:\Users\Sam")])).unwrap();
        assert_eq!(p.data_dir, PathBuf::from(r"C:\Users\Sam\AppData\Local\Local Tugboat"));
        assert_eq!(p.bin_dir, PathBuf::from(r"C:\Users\Sam\AppData\Local\Local Tugboat\Bin"));
        assert_eq!(p.default_library, PathBuf::from(r"C:\Users\Sam\Local Tugboat"));
        assert!(!p.default_library.to_string_lossy().contains("Pictures"));
        assert_eq!(p.key_user, "pc-secret-key");
    }

    #[test]
    fn a_profile_isolates_data_library_and_key() {
        let real = resolve(&env(&[("LOCALAPPDATA", r"C:\L"), ("USERPROFILE", r"C:\U")])).unwrap();
        let qa = resolve(&env(&[(PROFILE_ENV, r"C:\qa\one"), ("LOCALAPPDATA", r"C:\L"), ("USERPROFILE", r"C:\U")])).unwrap();
        let qa2 = resolve(&env(&[(PROFILE_ENV, r"C:\qa\two")])).unwrap();
        assert_eq!(qa.data_dir, PathBuf::from(r"C:\qa\one"));
        assert_eq!(qa.default_library, PathBuf::from(r"C:\qa\one\library"));
        assert_ne!(qa.key_user, real.key_user);
        assert_ne!(qa.key_user, qa2.key_user);
        assert!(resolve(&env(&[(PROFILE_ENV, "relative")])).is_err());
    }

    #[test]
    fn missing_variables_are_errors() {
        assert!(resolve(&env(&[])).is_err());
    }

    #[test]
    fn bind_defaults_to_all_interfaces_and_honours_the_qa_override() {
        assert_eq!(parse_bind(None, 47300).unwrap().to_string(), "0.0.0.0:47300");
        assert_eq!(parse_bind(Some(""), 47300).unwrap().to_string(), "0.0.0.0:47300");
        assert_eq!(parse_bind(Some("127.0.0.1"), 47301).unwrap().to_string(), "127.0.0.1:47301");
        assert_eq!(parse_bind(Some("127.0.0.1:5000"), 47301).unwrap().to_string(), "127.0.0.1:5000");
        assert!(parse_bind(Some("localhost"), 1).is_err());
    }

    #[test]
    fn a_pinned_bind_address_turns_mdns_off() {
        assert!(mdns_allowed(None));
        assert!(mdns_allowed(Some("  ")));
        assert!(!mdns_allowed(Some("127.0.0.1")));
        assert!(!mdns_allowed(Some("127.0.0.1:5000")));
    }

    #[test]
    fn the_firewall_check_is_skipped_only_on_a_qa_profile() {
        assert!(!firewall_check_allowed(true, Some("skip")));
        assert!(firewall_check_allowed(false, Some("skip")), "a real run always checks");
        assert!(firewall_check_allowed(true, None));
        assert!(firewall_check_allowed(true, Some("yes")));
    }
}
