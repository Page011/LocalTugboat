//! Adversarial tests for the user-scope part of the firewall verdict (verifier stage).
//! A failing test is a finding.
//!
//! The oracle is the Win32 AccessCheck of the current process token against the rule's user
//! scope, asking for access right 0x1 (FWP_ACTRL_MATCH_FILTER, the "CC" right the firewall
//! writes into LocalUserAuthorizedList). That is how WFP matches an ALE_USER_ID condition, so a
//! scope the oracle grants is a rule that really applies to Tugboat.

use std::ffi::c_void;
use std::path::Path;

use tugboat_desktop_lib::firewall::{evaluate_with, token_applies, FirewallStatus, ProfileInfo, RuleInfo, Snapshot, UserScope, PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC};

const EXE: &str = r"C:\Program Files\Local Tugboat\tugboat.exe";
const MATCH_FILTER: u32 = 0x1;

#[repr(C)]
struct GenericMapping {
    read: u32,
    write: u32,
    execute: u32,
    all: u32,
}

// Raw declarations so the test needs no extra `windows` crate features.
#[link(name = "advapi32")]
extern "system" {
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(s: *const u16, rev: u32, sd: *mut *mut c_void, len: *mut u32) -> i32;
    fn OpenProcessToken(process: isize, access: u32, token: *mut isize) -> i32;
    fn DuplicateToken(existing: isize, level: i32, new: *mut isize) -> i32;
    fn AccessCheck(sd: *mut c_void, token: isize, desired: u32, mapping: *const GenericMapping, privs: *mut c_void, privlen: *mut u32, granted: *mut u32, status: *mut i32) -> i32;
}
#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(h: *mut c_void) -> *mut c_void;
    fn CloseHandle(h: isize) -> i32;
    fn GetCurrentProcess() -> isize;
}

/// Does the real access check grant the current token the match right on `sddl`?
/// AccessCheck needs owner and group in the descriptor; the group never affects the DACL walk.
fn oracle_applies(sddl: &str) -> bool {
    let with_group = if sddl.contains("G:") {
        sddl.to_string()
    } else if let Some(i) = sddl.find("D:") {
        format!("{}G:LS{}", &sddl[..i], &sddl[i..])
    } else {
        format!("{sddl}G:LS")
    };
    let wide: Vec<u16> = with_group.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut sd: *mut c_void = std::ptr::null_mut();
        assert!(ConvertStringSecurityDescriptorToSecurityDescriptorW(wide.as_ptr(), 1, &mut sd, std::ptr::null_mut()) != 0, "oracle parse {with_group}");
        let mut primary = 0isize;
        assert!(OpenProcessToken(GetCurrentProcess(), 0x2 | 0x8, &mut primary) != 0);
        let mut imp = 0isize;
        assert!(DuplicateToken(primary, 2, &mut imp) != 0);
        let mapping = GenericMapping { read: 0, write: 0, execute: 0, all: MATCH_FILTER };
        let mut privs = [0u8; 256];
        let mut privlen = privs.len() as u32;
        let (mut granted, mut status) = (0u32, 0i32);
        let ok = AccessCheck(sd, imp, MATCH_FILTER, &mapping, privs.as_mut_ptr() as *mut c_void, &mut privlen, &mut granted, &mut status);
        CloseHandle(imp);
        CloseHandle(primary);
        LocalFree(sd);
        assert!(ok != 0, "AccessCheck call failed for {with_group}");
        status != 0
    }
}

fn profiles() -> Vec<ProfileInfo> {
    [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC].into_iter().map(|p| ProfileInfo { profile: p, enabled: true, default_inbound_blocks: true }).collect()
}

fn allow_rule(scope: UserScope) -> RuleInfo {
    RuleInfo { name: "Local Tugboat".into(), enabled: true, inbound: true, allow: true, app: EXE.into(), protocol: 17, profiles: PROFILE_PRIVATE | PROFILE_PUBLIC, user_scope: scope, ..RuleInfo::default() }
}

fn block_rule(scope: UserScope) -> RuleInfo {
    RuleInfo { name: "block".into(), enabled: true, inbound: true, allow: false, protocol: 256, profiles: i32::MAX, user_scope: scope, ..RuleInfo::default() }
}

fn judge(rules: Vec<RuleInfo>) -> FirewallStatus {
    let s = Snapshot { rules, profiles: profiles(), current_profiles: PROFILE_PRIVATE, modify_state: 0 };
    evaluate_with(&s, Path::new(EXE), Some(47300), &token_applies)
}

/// Scopes worth trying: deny/allow order, masks without the match right, deny-only groups,
/// empty and NULL DACLs, owner-only, inherit-only and object ACEs.
const SCOPES: &[&str] = &[
    "O:LSD:(A;;CC;;;WD)",
    "O:LSD:(A;;CC;;;S-1-5-21-1-2-3-4)(A;;CC;;;BU)",
    "O:LSD:(D;;CC;;;WD)(A;;CC;;;WD)",
    "O:LSD:(A;;CC;;;WD)(D;;CC;;;WD)",
    // A deny entry that does not carry the match right does not stop the allow after it.
    "O:LSD:(D;;0x2;;;WD)(A;;CC;;;WD)",
    "O:LSD:(D;;GA;;;WD)(A;;CC;;;WD)",
    "O:LSD:(D;;RC;;;AU)(A;;CC;;;AU)",
    // Administrators is deny-only in a filtered (non-elevated) token: deny ACEs still match it.
    "O:LSD:(D;;CC;;;BA)(A;;CC;;;WD)",
    "O:LSD:(A;;CC;;;BA)",
    "O:LSD:(D;IO;CC;;;WD)(A;;CC;;;WD)",
    "O:LSD:",
    "O:LSD:NO_ACCESS_CONTROL",
    "O:LS",
    "O:LSD:(OA;;CC;bf967a86-0de6-11d0-a285-00aa003049e2;;WD)",
    "O:LSD:(A;;0x2;;;WD)",
    "O:LSD:(A;;0x3;;;WD)",
    "O:LSD:(A;;CC;;;OW)",
];

/// The dangerous direction: a block rule whose scope really applies must be reported.
#[test]
fn a_block_rule_the_access_check_applies_to_us_is_never_hidden() {
    let mut hidden = Vec::new();
    for sddl in SCOPES {
        let applies = oracle_applies(sddl);
        let v = judge(vec![allow_rule(UserScope::All), block_rule(UserScope::Sddl((*sddl).into()))]);
        let reported = matches!(v, FirewallStatus::BlockedByRule { .. });
        println!("{sddl:<60} oracle_applies={applies:<5} verdict={v:?}");
        if applies && !reported {
            hidden.push(*sddl);
        }
    }
    assert!(hidden.is_empty(), "real blocks reported as fine: {hidden:?}");
}

/// The other dangerous direction: an allow rule must not be counted when its scope does not
/// really apply (the app would show Ok while the phone cannot connect).
#[test]
fn an_allow_rule_the_access_check_does_not_apply_to_us_is_not_counted() {
    let mut wrongly_counted = Vec::new();
    for sddl in SCOPES {
        let applies = oracle_applies(sddl);
        let v = judge(vec![allow_rule(UserScope::Sddl((*sddl).into()))]);
        println!("{sddl:<60} oracle_applies={applies:<5} verdict={v:?}");
        if !applies && v == FirewallStatus::Ok {
            wrongly_counted.push(*sddl);
        }
    }
    assert!(wrongly_counted.is_empty(), "allow rules counted that do not apply: {wrongly_counted:?}");
}

/// Safe-direction disagreements (false alarms), reported for information only.
#[test]
fn false_alarms_against_the_access_check_are_listed() {
    for sddl in SCOPES {
        let applies = oracle_applies(sddl);
        let v = judge(vec![allow_rule(UserScope::All), block_rule(UserScope::Sddl((*sddl).into()))]);
        if !applies && matches!(v, FirewallStatus::BlockedByRule { note: None, .. }) {
            println!("FALSE ALARM (no note): {sddl}");
        }
    }
}

/// The exact Codex scope, checked against the oracle too.
#[test]
fn the_codex_scope_does_not_apply_by_the_access_check_either() {
    let codex = "O:LSD:(A;;CC;;;S-1-5-21-255584919-3964267139-957157073-1006)";
    assert!(!oracle_applies(codex));
    assert_eq!(judge(vec![allow_rule(UserScope::All), block_rule(UserScope::Sddl(codex.into()))]), FirewallStatus::Ok);
}
