//! Adversarial tests for the firewall verdict (verifier stage). A failing test is a finding.

use std::path::Path;

use tugboat_desktop_lib::firewall::{evaluate, FirewallStatus, ProfileInfo, RuleInfo, Snapshot, PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC};

const EXE: &str = r"C:\Program Files\Local Tugboat\tugboat.exe";

fn profiles() -> Vec<ProfileInfo> {
    [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC].into_iter().map(|p| ProfileInfo { profile: p, enabled: true, default_inbound_blocks: true }).collect()
}

fn installer_rule() -> RuleInfo {
    RuleInfo { name: "Local Tugboat".into(), enabled: true, inbound: true, allow: true, app: EXE.into(), protocol: 17, profiles: PROFILE_PRIVATE | PROFILE_PUBLIC, ..RuleInfo::default() }
}

/// Windows Firewall: a block rule wins over any allow rule. A block rule that applies to every
/// program (empty application name) and to inbound UDP on the active profile blocks Tugboat,
/// but `evaluate` only looks at rules naming tugboat.exe, so it says Ok and no banner shows.
#[test]
fn an_all_programs_udp_block_rule_is_not_reported_as_fine() {
    let block_all = RuleInfo { name: "Block inbound UDP".into(), enabled: true, inbound: true, allow: false, app: String::new(), protocol: 17, profiles: PROFILE_PRIVATE, ..RuleInfo::default() };
    let s = Snapshot { rules: vec![installer_rule(), block_all], profiles: profiles(), current_profiles: PROFILE_PRIVATE, modify_state: 0 };
    assert_ne!(evaluate(&s, Path::new(EXE)), FirewallStatus::Ok);
}
