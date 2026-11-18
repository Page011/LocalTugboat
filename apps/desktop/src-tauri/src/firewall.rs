//! Windows Firewall check. The installer adds an inbound rule for the Tugboat program on the
//! Private and Public profiles; this module asks the firewall API (INetFwPolicy2) at run time
//! whether that rule exists AND takes effect, because group policy on a managed PC can ignore
//! local rules, and a rule that is silently ignored means a phone that can never connect.
//!
//! `evaluate_with` is pure (a snapshot in, a verdict out) so every verdict is unit tested;
//! `snapshot` is the only part that talks to COM.
//!
//! A rule only counts when it reaches the traffic we care about: inbound UDP to Tugboat's port
//! from a phone on the LAN, to a process running as the current user. So every rule is judged on
//! its profiles, program, service, app package, user scope, interface types, remote addresses and
//! local ports. A scope that cannot be read or understood is `Maybe`: a block rule with a `Maybe`
//! scope is reported (a real block must never be hidden), an allow rule with one is not counted
//! (a missing allow must never be hidden either).

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const RULE_NAME: &str = "Local Tugboat";

// NET_FW_PROFILE_TYPE2 bits.
pub const PROFILE_DOMAIN: i32 = 1;
pub const PROFILE_PRIVATE: i32 = 2;
pub const PROFILE_PUBLIC: i32 = 4;
// Protocol numbers in INetFwRule::Protocol.
const PROTO_UDP: i32 = 17;
const PROTO_ANY: i32 = 256;
// NET_FW_MODIFY_STATE.
#[cfg(test)]
const MODIFY_OK: i32 = 0;
const MODIFY_GP_OVERRIDE: i32 = 1;
const MODIFY_INBOUND_BLOCKED: i32 = 2;

/// Who a rule applies to (INetFwRule3::LocalUserAuthorizedList).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UserScope {
    /// No user restriction: the rule applies to every user.
    #[default]
    All,
    /// An SDDL string: the rule applies to processes whose token the DACL grants.
    Sddl(String),
    /// The property could not be read; the reason is shown if it decides the verdict.
    Unreadable(String),
}

/// One firewall rule as the API reports it. Every scope string is the raw text from
/// INetFwRule; an empty string means "no restriction" (the API also uses `*` and `All`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuleInfo {
    pub name: String,
    pub enabled: bool,
    pub inbound: bool,
    pub allow: bool,
    pub app: String,
    pub protocol: i32,
    pub profiles: i32,
    /// ServiceName: a rule bound to a Windows service does not apply to our program.
    pub service: String,
    /// LocalAppPackageId: a rule bound to an app package does not apply to a desktop program.
    pub package: String,
    pub user_scope: UserScope,
    /// InterfaceTypes, e.g. `All` or `RemoteAccess,Wireless,Lan`.
    pub interface_types: String,
    /// RemoteAddresses: `*`, addresses, `a-b` ranges, `addr/mask` subnets or keywords.
    pub remote_addresses: String,
    /// LocalPorts: `*`, numbers, `a-b` ranges or keywords such as `RPC`.
    pub local_ports: String,
    /// LocalAddresses: same format as `remote_addresses`; the PC's own addresses the rule covers.
    pub local_addresses: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileInfo {
    pub profile: i32,
    pub enabled: bool,
    pub default_inbound_blocks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub rules: Vec<RuleInfo>,
    pub profiles: Vec<ProfileInfo>,
    /// Bit mask of the profiles active now.
    pub current_profiles: i32,
    pub modify_state: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FirewallStatus {
    /// Inbound traffic to the app is allowed on every active profile.
    Ok,
    /// The firewall blocks by default and no enabled allow rule covers the app.
    RuleMissing,
    /// A rule explicitly blocks the app. `rule` is its display name; `note` says why the verdict
    /// is a guess when part of the rule's scope could not be read.
    BlockedByRule { rule: String, note: Option<String> },
    /// Group policy overrides local rules, so the installer's rule has no effect.
    PolicyOverride,
    /// The "block all incoming connections" setting is on.
    AllInboundBlocked,
    /// The check itself failed. Shown as unknown, never as fine.
    Unknown { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct FirewallInfo {
    pub status: FirewallStatus,
    /// One short line for the banner; None when everything is fine.
    pub message: Option<String>,
    /// True when adding the rule (the [Fix] button) can help.
    pub can_fix: bool,
}

impl FirewallInfo {
    pub fn from_status(status: FirewallStatus) -> FirewallInfo {
        let (message, can_fix) = match &status {
            FirewallStatus::Ok => (None, false),
            FirewallStatus::RuleMissing => (Some("WINDOWS FIREWALL IS BLOCKING TUGBOAT".to_string()), true),
            FirewallStatus::BlockedByRule { rule, note } => {
                let mut m = format!("FIREWALL RULE \"{rule}\" BLOCKS TUGBOAT - change or remove it in Windows Firewall");
                if let Some(n) = note {
                    m.push_str(&format!(" (not sure: {n})"));
                }
                (Some(m), false)
            }
            FirewallStatus::PolicyOverride => (Some("FIREWALL POLICY IGNORES TUGBOAT'S RULE - ask your IT admin".to_string()), false),
            FirewallStatus::AllInboundBlocked => (Some("FIREWALL BLOCKS ALL INCOMING CONNECTIONS - turn that setting off".to_string()), false),
            FirewallStatus::Unknown { message } => (Some(format!("FIREWALL CHECK FAILED - {message}")), false),
        };
        FirewallInfo { status, message, can_fix }
    }
}

fn norm(p: &str) -> String {
    p.replace('/', "\\").to_lowercase()
}

// ---------- scope matching ----------

/// Does one part of a rule's scope reach our traffic?
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fit {
    Yes,
    No,
    /// Cannot tell; the text says why.
    Maybe(String),
}

/// Does a rule with this user-scope SDDL apply to the current process token? `Err` = the check
/// itself failed (the text is shown if it decides the verdict). Injectable so a test can force
/// a failure; the real one is `token_applies`.
pub type ScopeFn<'a> = &'a dyn Fn(&str) -> Result<bool, String>;

/// Combine parts: any `No` wins, then any `Maybe`, else `Yes`. Parts run lazily in order so the
/// cheap ones can rule a rule out before the Win32 calls of the user scope.
fn all_of(parts: &[&dyn Fn() -> Fit]) -> Fit {
    let mut maybe: Option<String> = None;
    for p in parts {
        match p() {
            Fit::No => return Fit::No,
            Fit::Maybe(why) => {
                maybe.get_or_insert(why);
            }
            Fit::Yes => {}
        }
    }
    maybe.map_or(Fit::Yes, Fit::Maybe)
}

fn is_any(spec: &str) -> bool {
    let t = spec.trim();
    t.is_empty() || t == "*" || t.eq_ignore_ascii_case("all")
}

/// Windows Firewall interface types: a phone reaches the PC over Wi-Fi or Ethernet.
fn interface_fit(spec: &str) -> Fit {
    if is_any(spec) {
        return Fit::Yes;
    }
    let hit = spec.split(',').map(|t| t.trim().to_ascii_lowercase()).any(|t| t == "lan" || t == "wireless" || t == "all");
    if hit {
        Fit::Yes
    } else {
        Fit::No
    }
}

/// Address ranges a phone can have on the LAN (IPv4: Tugboat binds 0.0.0.0): private ranges,
/// link-local, and 100.64/10 (carrier NAT, which Tailscale uses).
const LAN_V4: [(u32, u32); 5] = [
    (0x0A00_0000, 0x0AFF_FFFF),
    (0xAC10_0000, 0xAC1F_FFFF),
    (0xC0A8_0000, 0xC0A8_FFFF),
    (0xA9FE_0000, 0xA9FE_FFFF),
    (0x6440_0000, 0x647F_FFFF),
];

/// One RemoteAddresses token as an IPv4 range. `Ok(None)` = an IPv6 token (our socket is IPv4).
fn v4_range(tok: &str) -> Result<Option<(u32, u32)>, String> {
    use std::net::Ipv4Addr;
    let bad = || format!("cannot read remote address \"{tok}\"");
    if tok.contains(':') {
        return Ok(None);
    }
    let addr = |t: &str| t.trim().parse::<Ipv4Addr>().map(u32::from).map_err(|_| bad());
    if let Some((a, b)) = tok.split_once('-') {
        let (a, b) = (addr(a)?, addr(b)?);
        return if a <= b { Ok(Some((a, b))) } else { Err(bad()) };
    }
    if let Some((a, m)) = tok.split_once('/') {
        let a = addr(a)?;
        let mask = if m.contains('.') {
            let mask = addr(m)?;
            if mask.leading_ones() + mask.trailing_zeros() != 32 {
                return Err(bad());
            }
            mask
        } else {
            match m.trim().parse::<u32>() {
                Ok(0) => 0,
                Ok(p) if p <= 32 => u32::MAX << (32 - p),
                _ => return Err(bad()),
            }
        };
        let start = a & mask;
        return Ok(Some((start, start | !mask)));
    }
    let a = addr(tok)?;
    Ok(Some((a, a)))
}

/// Can the rule's remote addresses include a phone on the LAN? A block rule matches when any
/// LAN address is in it (a block must never be hidden). An allow rule counts only when it takes
/// every LAN phone: an explicit address list is `Maybe`, which an allow path does not count.
fn remote_fit(spec: &str, allow: bool) -> Fit {
    if is_any(spec) {
        return Fit::Yes;
    }
    let mut maybe: Option<String> = None;
    for tok in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match tok.to_ascii_lowercase().as_str() {
            "*" | "localsubnet" | "intranet" | "rmtintranet" => return Fit::Yes,
            // Named infrastructure hosts and the public internet are never the phone.
            "internet" | "dns" | "dhcp" | "wins" | "defaultgateway" => continue,
            k if k.chars().all(|c| c.is_ascii_alphabetic()) => {
                maybe.get_or_insert(format!("unknown remote address keyword \"{tok}\""));
            }
            _ => match v4_range(tok) {
                Ok(Some((s, e))) => {
                    if LAN_V4.iter().any(|&(ps, pe)| s <= pe && e >= ps) {
                        if !allow {
                            return Fit::Yes;
                        }
                        maybe.get_or_insert("the rule allows only some addresses".to_string());
                    }
                }
                Ok(None) => {}
                Err(why) => {
                    maybe.get_or_insert(why);
                }
            },
        }
    }
    maybe.map_or(Fit::No, Fit::Maybe)
}

/// Does the rule cover every local address of this PC? Only an allow rule is judged on it: an
/// allow limited to some local addresses (say 127.0.0.1) is `Maybe`, so it is not counted.
/// A block rule keeps ignoring it (a possible block stays reported).
fn local_addr_fit(spec: &str, allow: bool) -> Fit {
    if !allow || is_any(spec) {
        Fit::Yes
    } else {
        Fit::Maybe("the rule covers only some of this PC's addresses".into())
    }
}

/// Do the rule's local ports include Tugboat's UDP port?
fn ports_fit(spec: &str, port: Option<u16>) -> Fit {
    if is_any(spec) {
        return Fit::Yes;
    }
    let Some(port) = port else { return Fit::Maybe("the app's port is not known".into()) };
    let mut maybe: Option<String> = None;
    for tok in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let one = |t: &str| t.trim().parse::<u16>();
        let hit = match tok.split_once('-') {
            Some((a, b)) => match (one(a), one(b)) {
                (Ok(a), Ok(b)) => Some(a <= port && port <= b),
                _ => None,
            },
            None => one(tok).ok().map(|p| p == port),
        };
        match hit {
            Some(true) => return Fit::Yes,
            Some(false) => {}
            // A keyword such as RPC or Teredo names fixed ports that are not ours.
            None if tok.chars().all(|c| c.is_ascii_alphabetic() || c == '-') => {}
            None => {
                maybe.get_or_insert(format!("cannot read local port \"{tok}\""));
            }
        }
    }
    maybe.map_or(Fit::No, Fit::Maybe)
}

/// Does the rule apply to the current user? The rule's user scope is an SDDL string (a DACL of
/// the users it applies to). Windows decides with an access check of the user's token against
/// it, so `applies` runs the same check instead of walking the entries by hand: that handles
/// access masks, deny entries, deny-only groups (a non-elevated administrator), inherit-only
/// entries, NULL and empty lists exactly as Windows does.
fn user_fit(scope: &UserScope, applies: ScopeFn) -> Fit {
    let sddl = match scope {
        UserScope::All => return Fit::Yes,
        UserScope::Unreadable(why) => return Fit::Maybe(why.clone()),
        UserScope::Sddl(s) if s.trim().is_empty() => return Fit::Yes,
        UserScope::Sddl(s) => s,
    };
    match applies(sddl) {
        Ok(true) => Fit::Yes,
        Ok(false) => Fit::No,
        Err(why) => Fit::Maybe(why),
    }
}

/// The access right a user-scope entry grants: FWP_ACTRL_MATCH_FILTER, written "CC" in SDDL.
/// WFP matches an ALE_USER_ID condition by asking for exactly this right on the token.
const MATCH_FILTER: u32 = 0x1;

/// The real check: would Windows' access check grant the current process token the match right
/// on `sddl`? Uses an impersonation copy of the token (AccessCheck refuses a primary token).
pub fn token_applies(sddl: &str) -> Result<bool, String> {
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
            Security::{
                AccessCheck,
                Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1},
                DuplicateToken, GetSecurityDescriptorDacl, GetSecurityDescriptorGroup, GetSecurityDescriptorOwner, InitializeSecurityDescriptor,
                SecurityImpersonation, SetSecurityDescriptorDacl, SetSecurityDescriptorGroup, SetSecurityDescriptorOwner, ACL,
                GENERIC_MAPPING, PRIVILEGE_SET, PSECURITY_DESCRIPTOR, SECURITY_DESCRIPTOR, TOKEN_DUPLICATE, TOKEN_QUERY,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };
    /// Frees a descriptor Windows allocated, on every exit path.
    struct LocalMem(*mut c_void);
    impl Drop for LocalMem {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(Some(HLOCAL(self.0))) };
            }
        }
    }
    /// Closes a token handle on every exit path.
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                let _ = unsafe { CloseHandle(self.0) };
            }
        }
    }
    let scope_err = |e: windows::core::Error| format!("cannot read the rule's user scope ({e})");
    let check_err = |e: windows::core::Error| format!("cannot check the rule's user scope against the current user ({e})");
    let parse = |text: &str| -> windows::core::Result<(LocalMem, PSECURITY_DESCRIPTOR)> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(PCWSTR(wide.as_ptr()), SDDL_REVISION_1, &mut sd, None) }?;
        Ok((LocalMem(sd.0), sd))
    };

    let (_rule_mem, rule_sd) = parse(sddl).map_err(scope_err)?;
    // AccessCheck wants an owner and a group, and a firewall scope usually has only a list.
    // Rebuild the descriptor with the rule's own owner/group where it has them (OWNER RIGHTS
    // depends on the owner), else Local System, which is never the user.
    let (_fallback_mem, fallback_sd) = parse("O:SYG:SY").map_err(scope_err)?;
    let (mut owner, mut group) = (windows::Win32::Security::PSID::default(), windows::Win32::Security::PSID::default());
    let (mut present, mut defaulted) = (windows::core::BOOL(0), windows::core::BOOL(0));
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorOwner(rule_sd, &mut owner, &mut defaulted).map_err(scope_err)?;
        GetSecurityDescriptorGroup(rule_sd, &mut group, &mut defaulted).map_err(scope_err)?;
        if owner.0.is_null() {
            GetSecurityDescriptorOwner(fallback_sd, &mut owner, &mut defaulted).map_err(scope_err)?;
        }
        if group.0.is_null() {
            GetSecurityDescriptorGroup(fallback_sd, &mut group, &mut defaulted).map_err(scope_err)?;
        }
        GetSecurityDescriptorDacl(rule_sd, &mut present, &mut dacl, &mut defaulted).map_err(scope_err)?;
    }
    let mut abs = SECURITY_DESCRIPTOR::default();
    let abs_sd = PSECURITY_DESCRIPTOR(&mut abs as *mut SECURITY_DESCRIPTOR as *mut c_void);
    unsafe {
        InitializeSecurityDescriptor(abs_sd, 1).map_err(scope_err)?;
        SetSecurityDescriptorOwner(abs_sd, Some(owner), false).map_err(scope_err)?;
        SetSecurityDescriptorGroup(abs_sd, Some(group), false).map_err(scope_err)?;
        // Keeps "no list" and "NULL list" as the rule had them; AccessCheck treats both as open.
        SetSecurityDescriptorDacl(abs_sd, present.as_bool(), Some(dacl as *const ACL), false).map_err(scope_err)?;
    }

    let mut primary = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY, &mut primary) }.map_err(check_err)?;
    let primary = Handle(primary);
    let mut imp = HANDLE::default();
    unsafe { DuplicateToken(primary.0, SecurityImpersonation, &mut imp) }.map_err(check_err)?;
    let imp = Handle(imp);

    let mapping = GENERIC_MAPPING { GenericRead: 0, GenericWrite: 0, GenericExecute: 0, GenericAll: MATCH_FILTER };
    // Four-byte aligned, as PRIVILEGE_SET needs; no privilege is requested, so it stays unused.
    let mut privs = [0u32; 64];
    let mut privs_len = std::mem::size_of_val(&privs) as u32;
    let (mut granted, mut status) = (0u32, windows::core::BOOL(0));
    unsafe { AccessCheck(abs_sd, imp.0, MATCH_FILTER, &mapping, Some(privs.as_mut_ptr() as *mut PRIVILEGE_SET), &mut privs_len, &mut granted, &mut status) }
        .map_err(check_err)?;
    Ok(status.as_bool())
}

/// Does `r` reach Tugboat's traffic (inbound UDP from a LAN phone, to the program at `exe_n`
/// running as the current user)? Profile and protocol are checked by the caller.
fn rule_fit(r: &RuleInfo, exe_n: &str, port: Option<u16>, applies: ScopeFn) -> Fit {
    all_of(&[
        // A rule with no program name applies to every program.
        &|| if r.app.is_empty() || norm(&r.app) == exe_n { Fit::Yes } else { Fit::No },
        &|| if r.service.trim().is_empty() { Fit::Yes } else { Fit::No },
        &|| if r.package.trim().is_empty() { Fit::Yes } else { Fit::No },
        &|| interface_fit(&r.interface_types),
        &|| remote_fit(&r.remote_addresses, r.allow),
        &|| local_addr_fit(&r.local_addresses, r.allow),
        &|| ports_fit(&r.local_ports, port),
        &|| user_fit(&r.user_scope, applies),
    ])
}

/// The verdict for the program at `exe` listening on UDP `port` (None = not known), judged with
/// the real access check.
pub fn evaluate_with_port(s: &Snapshot, exe: &Path, port: Option<u16>) -> FirewallStatus {
    evaluate_with(s, exe, port, &token_applies)
}

/// The verdict for the program at `exe`, port not known.
pub fn evaluate(s: &Snapshot, exe: &Path) -> FirewallStatus {
    evaluate_with_port(s, exe, None)
}

/// The verdict, with the access check injected.
pub fn evaluate_with(s: &Snapshot, exe: &Path, port: Option<u16>, applies: ScopeFn) -> FirewallStatus {
    let exe_n = norm(&exe.to_string_lossy());
    let covers = |r: &RuleInfo, bit: i32| r.profiles & bit != 0 && (r.protocol == PROTO_UDP || r.protocol == PROTO_ANY);

    if s.modify_state == MODIFY_INBOUND_BLOCKED {
        return FirewallStatus::AllInboundBlocked;
    }
    // A profile needs a rule only when the firewall is on there and blocks inbound by default.
    let needing: Vec<i32> = [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC]
        .into_iter()
        .filter(|bit| s.current_profiles & bit != 0)
        .filter(|bit| s.profiles.iter().find(|p| p.profile == *bit).is_none_or(|p| p.enabled && p.default_inbound_blocks))
        .collect();

    // An explicit block rule beats allow rules on every profile it covers.
    for bit in [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC] {
        let active = s.current_profiles & bit != 0 && s.profiles.iter().find(|p| p.profile == bit).is_none_or(|p| p.enabled);
        if !active {
            continue;
        }
        for r in s.rules.iter().filter(|r| r.enabled && r.inbound && !r.allow && covers(r, bit)) {
            match rule_fit(r, &exe_n, port, applies) {
                Fit::No => {}
                Fit::Yes => return FirewallStatus::BlockedByRule { rule: r.name.clone(), note: None },
                Fit::Maybe(why) => return FirewallStatus::BlockedByRule { rule: r.name.clone(), note: Some(why) },
            }
        }
    }
    if needing.is_empty() {
        return FirewallStatus::Ok;
    }
    if s.modify_state == MODIFY_GP_OVERRIDE {
        return FirewallStatus::PolicyOverride;
    }
    // An allow rule counts only when it certainly reaches us and names our program.
    let allowed = |bit: i32| {
        s.rules
            .iter()
            .any(|r| r.enabled && r.inbound && r.allow && !r.app.is_empty() && covers(r, bit) && rule_fit(r, &exe_n, port, applies) == Fit::Yes)
    };
    if needing.iter().all(|b| allowed(*b)) {
        FirewallStatus::Ok
    } else {
        FirewallStatus::RuleMissing
    }
}

/// Profiles the [Fix] rule covers: Private and Public always (as the installer does), plus
/// Domain when this PC is on a domain network now (else the banner could never clear).
pub fn fix_profiles(current: i32) -> Vec<&'static str> {
    let mut v = vec!["private", "public"];
    if current & PROFILE_DOMAIN != 0 {
        v.push("domain");
    }
    v
}

/// The `netsh` argument line that adds the allow rule for `exe`.
pub fn fix_arguments(exe: &Path, current: i32) -> Result<String, String> {
    let exe = exe.to_string_lossy();
    // A Windows path cannot hold a double quote; refuse rather than build a broken command line.
    if exe.contains('"') || exe.contains('\n') || exe.contains('\r') {
        return Err("THE PROGRAM PATH HAS INVALID CHARACTERS".into());
    }
    Ok(format!(
        "advfirewall firewall add rule name=\"{RULE_NAME}\" dir=in action=allow program=\"{exe}\" enable=yes profile={} protocol=UDP",
        fix_profiles(current).join(",")
    ))
}

pub fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("cannot find the program path: {e}"))
}

// ---------- COM ----------

use windows::{
    core::{Interface, BSTR},
    Win32::{
        Foundation::VARIANT_BOOL,
        NetworkManagement::WindowsFirewall::{
            INetFwPolicy2, INetFwRule, INetFwRule3, NetFwPolicy2, NET_FW_ACTION_ALLOW, NET_FW_PROFILE_TYPE2, NET_FW_RULE_DIR_IN,
        },
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, IDispatch, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED},
            Ole::IEnumVARIANT,
            Variant::VARIANT,
        },
    },
};

fn werr(what: &str, e: windows::core::Error) -> String {
    format!("{what}: {e}")
}

/// Read the firewall state through INetFwPolicy2. Blocking (hundreds of rules): call it off
/// the UI thread.
pub fn snapshot() -> Result<Snapshot, String> {
    let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let r = read_snapshot();
    if com_ok {
        unsafe { CoUninitialize() };
    }
    r
}

fn read_snapshot() -> Result<Snapshot, String> {
    let policy: INetFwPolicy2 = unsafe { CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER) }.map_err(|e| werr("firewall policy", e))?;
    let current_profiles = unsafe { policy.CurrentProfileTypes() }.map_err(|e| werr("current profiles", e))?;
    let modify_state = unsafe { policy.LocalPolicyModifyState() }.map_err(|e| werr("policy state", e))?.0;
    let mut profiles = Vec::new();
    for bit in [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC] {
        let t = NET_FW_PROFILE_TYPE2(bit);
        let enabled: VARIANT_BOOL = unsafe { policy.get_FirewallEnabled(t) }.map_err(|e| werr("firewall enabled", e))?;
        let action = unsafe { policy.get_DefaultInboundAction(t) }.map_err(|e| werr("default inbound action", e))?;
        profiles.push(ProfileInfo { profile: bit, enabled: enabled.as_bool(), default_inbound_blocks: action != NET_FW_ACTION_ALLOW });
    }
    let rules_coll = unsafe { policy.Rules() }.map_err(|e| werr("rules", e))?;
    let unk = unsafe { rules_coll._NewEnum() }.map_err(|e| werr("rule list", e))?;
    let enumr: IEnumVARIANT = unk.cast().map_err(|e| werr("rule enumerator", e))?;
    let mut rules = Vec::new();
    loop {
        let mut item = [VARIANT::default()];
        let mut fetched = 0u32;
        let hr = unsafe { enumr.Next(&mut item, &mut fetched) };
        // A failed read is an error, not the end of the list: a partial list could hide a block rule.
        if hr.is_err() {
            return Err(werr("rule list", windows::core::Error::from(hr)));
        }
        if fetched == 0 {
            break;
        }
        let disp = IDispatch::try_from(&item[0]).map_err(|e| werr("rule item", e))?;
        let rule = disp.cast::<INetFwRule>().map_err(|e| werr("rule item", e))?;
        // INetFwRule3 (Windows 8 and later) carries the user scope and the app package.
        let rule3 = disp.cast::<INetFwRule3>();
        // Cosmetic only (the rule's display name): a failed read may fall back to "".
        let name = unsafe { rule.Name() }.map(|b| b.to_string()).unwrap_or_default();
        // Every field the verdict reads propagates its error: "" means "any" / "no restriction"
        // to the evaluator, so a failed read must never become "" (an allow rule would count as
        // full cover).
        let text = |what: &'static str, r: windows::core::Result<BSTR>| r.map(|b| b.to_string()).map_err(|e| werr(what, e));
        // The fields that decide the verdict propagate their errors: a failed read is Unknown,
        // never a guessed false/0 that could hide a block rule or count a wrong one.
        let enabled = unsafe { rule.Enabled() }.map_err(|e| werr("rule enabled", e))?.as_bool();
        let inbound = unsafe { rule.Direction() }.map_err(|e| werr("rule direction", e))? == NET_FW_RULE_DIR_IN;
        let allow = unsafe { rule.Action() }.map_err(|e| werr("rule action", e))? == NET_FW_ACTION_ALLOW;
        let protocol = unsafe { rule.Protocol() }.map_err(|e| werr("rule protocol", e))?;
        let rule_profiles = unsafe { rule.Profiles() }.map_err(|e| werr("rule profiles", e))?;
        let local_addresses = text("rule local addresses", unsafe { rule.LocalAddresses() })?;
        let app = text("rule program", unsafe { rule.ApplicationName() })?;
        let service = text("rule service", unsafe { rule.ServiceName() })?;
        // Without INetFwRule3 (before Windows 8) there is no package restriction: "" is right.
        let package = match &rule3 {
            Ok(r) => text("rule app package", unsafe { r.LocalAppPackageId() })?,
            Err(_) => String::new(),
        };
        let interface_types = text("rule interface types", unsafe { rule.InterfaceTypes() })?;
        let remote_addresses = text("rule remote addresses", unsafe { rule.RemoteAddresses() })?;
        let local_ports = text("rule local ports", unsafe { rule.LocalPorts() })?;
        rules.push(RuleInfo {
            name,
            enabled,
            inbound,
            allow,
            app,
            protocol,
            profiles: rule_profiles,
            local_addresses,
            service,
            package,
            user_scope: match &rule3 {
                Ok(r) => match unsafe { r.LocalUserAuthorizedList() } {
                    Ok(b) => UserScope::Sddl(b.to_string()),
                    Err(e) => UserScope::Unreadable(format!("cannot read the rule's user scope ({e})")),
                },
                Err(e) => UserScope::Unreadable(format!("cannot read the rule's user scope ({e})")),
            },
            interface_types,
            remote_addresses,
            local_ports,
        });
    }
    Ok(Snapshot { rules, profiles, current_profiles, modify_state })
}

/// Ask the firewall and judge, for the program listening on UDP `port` (None = not known).
/// A failure is `Unknown`, never `Ok`.
pub fn check(port: Option<u16>) -> FirewallInfo {
    let status = match (current_exe(), snapshot()) {
        (Ok(exe), Ok(s)) => evaluate_with_port(&s, &exe, port),
        (Err(e), _) | (_, Err(e)) => FirewallStatus::Unknown { message: e },
    };
    FirewallInfo::from_status(status)
}

/// Full path of `netsh.exe` in the Windows system folder. ShellExecute with a bare name can
/// search the current directory, so a planted netsh.exe would run elevated; never fall back to
/// the bare name.
pub fn netsh_path() -> Result<PathBuf, String> {
    use windows::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{FOLDERID_System, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
    };
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_System, KF_FLAG_DEFAULT, None) }.map_err(|e| werr("Windows system folder", e))?;
    let dir = unsafe { raw.to_string() };
    unsafe { CoTaskMemFree(Some(raw.0 as *const c_void)) };
    let dir = dir.map_err(|_| "cannot read the Windows system folder path".to_string())?;
    Ok(PathBuf::from(dir).join("netsh.exe"))
}

/// The [Fix] button: run `netsh` elevated (a UAC prompt) to add the allow rule. Returns once the
/// request was handed to Windows; the caller re-checks after a moment.
pub fn fix() -> Result<(), String> {
    use windows::{
        core::PCWSTR,
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_HIDE},
    };
    let exe = current_exe()?;
    let current = snapshot().map(|s| s.current_profiles).unwrap_or(PROFILE_PRIVATE | PROFILE_PUBLIC);
    let args = fix_arguments(&exe, current)?;
    let netsh = netsh_path()?;
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (verb, file, params) = (wide("runas"), wide(&netsh.to_string_lossy()), wide(&args));
    let h = unsafe { ShellExecuteW(None, PCWSTR(verb.as_ptr()), PCWSTR(file.as_ptr()), PCWSTR(params.as_ptr()), PCWSTR::null(), SW_HIDE) };
    // ShellExecute reports success with a value above 32.
    if h.0 as isize > 32 {
        Ok(())
    } else {
        Err("THE FIREWALL RULE WAS NOT ADDED - the administrator prompt was declined".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Program Files\Local Tugboat\tugboat.exe";

    fn rule(profiles: i32) -> RuleInfo {
        RuleInfo { name: "Local Tugboat".into(), enabled: true, inbound: true, allow: true, app: EXE.into(), protocol: PROTO_UDP, profiles, ..RuleInfo::default() }
    }

    fn profiles(enabled: bool, blocks: bool) -> Vec<ProfileInfo> {
        [PROFILE_DOMAIN, PROFILE_PRIVATE, PROFILE_PUBLIC].into_iter().map(|p| ProfileInfo { profile: p, enabled, default_inbound_blocks: blocks }).collect()
    }

    fn snap(rules: Vec<RuleInfo>, current: i32) -> Snapshot {
        Snapshot { rules, profiles: profiles(true, true), current_profiles: current, modify_state: MODIFY_OK }
    }

    fn verdict(s: &Snapshot) -> FirewallStatus {
        evaluate(s, Path::new(EXE))
    }

    #[test]
    fn the_installers_rule_makes_private_and_public_fine() {
        let s = snap(vec![rule(PROFILE_PRIVATE | PROFILE_PUBLIC)], PROFILE_PRIVATE);
        assert_eq!(verdict(&s), FirewallStatus::Ok);
        assert_eq!(verdict(&snap(vec![rule(PROFILE_PRIVATE | PROFILE_PUBLIC)], PROFILE_PUBLIC)), FirewallStatus::Ok);
    }

    #[test]
    fn no_rule_on_a_blocking_firewall_is_the_banner_case() {
        assert_eq!(verdict(&snap(vec![], PROFILE_PRIVATE)), FirewallStatus::RuleMissing);
        assert!(FirewallInfo::from_status(FirewallStatus::RuleMissing).can_fix);
    }

    #[test]
    fn a_rule_that_does_not_cover_the_active_profile_is_missing() {
        // Domain network, rule only for Private+Public.
        assert_eq!(verdict(&snap(vec![rule(PROFILE_PRIVATE | PROFILE_PUBLIC)], PROFILE_DOMAIN)), FirewallStatus::RuleMissing);
    }

    #[test]
    fn disabled_wrong_program_wrong_protocol_or_outbound_rules_do_not_count() {
        let base = rule(PROFILE_PRIVATE);
        for bad in [
            RuleInfo { enabled: false, ..base.clone() },
            RuleInfo { app: r"C:\other\tugboat.exe".into(), ..base.clone() },
            RuleInfo { protocol: 6, ..base.clone() },
            RuleInfo { inbound: false, ..base.clone() },
        ] {
            assert_eq!(verdict(&snap(vec![bad], PROFILE_PRIVATE)), FirewallStatus::RuleMissing);
        }
        // Program path compare ignores case and slash direction; "any protocol" counts.
        let loose = RuleInfo { app: EXE.to_uppercase().replace('\\', "/"), protocol: PROTO_ANY, ..base };
        assert_eq!(verdict(&snap(vec![loose], PROFILE_PRIVATE)), FirewallStatus::Ok);
    }

    #[test]
    fn a_firewall_that_is_off_or_allows_inbound_needs_no_rule() {
        let mut s = snap(vec![], PROFILE_PRIVATE);
        s.profiles = profiles(false, true);
        assert_eq!(verdict(&s), FirewallStatus::Ok);
        s.profiles = profiles(true, false);
        assert_eq!(verdict(&s), FirewallStatus::Ok);
    }

    #[test]
    fn group_policy_override_means_the_local_rule_is_ignored() {
        let mut s = snap(vec![rule(PROFILE_PRIVATE | PROFILE_PUBLIC)], PROFILE_PRIVATE);
        s.modify_state = MODIFY_GP_OVERRIDE;
        assert_eq!(verdict(&s), FirewallStatus::PolicyOverride);
        assert!(!FirewallInfo::from_status(FirewallStatus::PolicyOverride).can_fix);
        // Policy override does not matter when the firewall allows inbound anyway.
        s.profiles = profiles(true, false);
        assert_eq!(verdict(&s), FirewallStatus::Ok);
    }

    #[test]
    fn block_all_incoming_and_explicit_block_rules_are_named() {
        let mut s = snap(vec![rule(PROFILE_PRIVATE)], PROFILE_PRIVATE);
        s.modify_state = MODIFY_INBOUND_BLOCKED;
        assert_eq!(verdict(&s), FirewallStatus::AllInboundBlocked);
        let block = RuleInfo { allow: false, ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert_eq!(verdict(&snap(vec![rule(PROFILE_PRIVATE), block], PROFILE_PRIVATE)), FirewallStatus::BlockedByRule { rule: "Local Tugboat".into(), note: None });
    }

    #[test]
    fn fix_command_line_covers_the_active_profile_and_refuses_bad_paths() {
        let a = fix_arguments(Path::new(EXE), PROFILE_PRIVATE).unwrap();
        assert_eq!(
            a,
            "advfirewall firewall add rule name=\"Local Tugboat\" dir=in action=allow program=\"C:\\Program Files\\Local Tugboat\\tugboat.exe\" enable=yes profile=private,public protocol=UDP"
        );
        assert!(fix_arguments(Path::new(EXE), PROFILE_DOMAIN).unwrap().contains("profile=private,public,domain"));
        assert!(fix_arguments(Path::new("C:\\a\"b\\x.exe"), PROFILE_PRIVATE).is_err());
    }

    #[test]
    fn netsh_is_run_from_the_absolute_system_folder() {
        let p = netsh_path().expect("system folder");
        assert!(p.is_absolute(), "{p:?}");
        assert!(p.to_string_lossy().to_lowercase().ends_with("system32\\netsh.exe"), "{p:?}");
        assert!(p.exists(), "{p:?}");
    }

    #[test]
    fn an_allow_limited_to_some_remote_addresses_is_not_full_cover() {
        let one = RuleInfo { remote_addresses: "192.168.1.10".into(), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert_eq!(verdict(&snap(vec![one], PROFILE_PRIVATE)), FirewallStatus::RuleMissing);
        // The same list on a block rule is still a block.
        let blocked = RuleInfo { allow: false, remote_addresses: "192.168.1.10".into(), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert!(matches!(verdict(&snap(vec![rule(PROFILE_PRIVATE), blocked], PROFILE_PRIVATE)), FirewallStatus::BlockedByRule { .. }));
        for any in ["LocalSubnet", "*", "Intranet", "10.0.0.5,LocalSubnet"] {
            let ok = RuleInfo { remote_addresses: any.into(), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
            assert_eq!(verdict(&snap(vec![ok], PROFILE_PRIVATE)), FirewallStatus::Ok, "{any}");
        }
    }

    #[test]
    fn an_allow_limited_to_some_local_addresses_is_not_counted() {
        let lo = RuleInfo { local_addresses: "127.0.0.1".into(), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert_eq!(verdict(&snap(vec![lo], PROFILE_PRIVATE)), FirewallStatus::RuleMissing);
        let any = RuleInfo { local_addresses: "*".into(), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert_eq!(verdict(&snap(vec![any], PROFILE_PRIVATE)), FirewallStatus::Ok);
        // A block rule limited to 127.0.0.1 is still reported (a possible block is never hidden).
        let block = RuleInfo { allow: false, local_addresses: "127.0.0.1".into(), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert!(matches!(verdict(&snap(vec![rule(PROFILE_PRIVATE), block], PROFILE_PRIVATE)), FirewallStatus::BlockedByRule { .. }));
    }

    #[test]
    fn unknown_is_never_presented_as_fine() {
        let i = FirewallInfo::from_status(FirewallStatus::Unknown { message: "x".into() });
        assert!(i.message.is_some() && !i.can_fix);
        assert!(FirewallInfo::from_status(FirewallStatus::Ok).message.is_none());
    }

    // ---------- scope tests ----------

    /// A SID no account here has, in the shape of a local account (the kind a sandbox account rule names).
    const OTHER_USER: &str = "S-1-5-21-1000000001-1000000002-1000000003-1006";
    /// The user scope Windows reports for a rule limited to one local sandbox account.
    const OTHER_USER_SDDL: &str = "O:LSD:(A;;CC;;;S-1-5-21-1000000001-1000000002-1000000003-1006)";

    /// The SID text of the user running the tests (the token's user, not a group).
    fn my_sid() -> String {
        use windows::{
            core::PWSTR,
            Win32::{
                Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
                Security::{Authorization::ConvertSidToStringSidW, GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER},
                System::Threading::{GetCurrentProcess, OpenProcessToken},
            },
        };
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).expect("open token");
            let mut buf = [0u64; 64];
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr() as *mut c_void), std::mem::size_of_val(&buf) as u32, &mut len).expect("token user");
            let _ = CloseHandle(token);
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut out = PWSTR::null();
            ConvertSidToStringSidW(user.User.Sid, &mut out).expect("sid to text");
            let s = out.to_string().expect("utf16");
            LocalFree(Some(HLOCAL(out.0 as *mut c_void)));
            s
        }
    }

    /// An inbound block rule limited to another local account, as the firewall API reports it, with the given user scope.
    fn other_user_inbound(scope: UserScope) -> RuleInfo {
        RuleInfo {
            name: "sandbox_offline_block_inbound".into(),
            enabled: true,
            inbound: true,
            allow: false,
            app: String::new(),
            protocol: PROTO_ANY,
            profiles: i32::MAX,
            interface_types: "All".into(),
            remote_addresses: "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff".into(),
            local_ports: "*".into(),
            user_scope: scope,
            ..RuleInfo::default()
        }
    }

    fn with_allow(block: RuleInfo) -> Snapshot {
        snap(vec![rule(PROFILE_PRIVATE | PROFILE_PUBLIC), block], PROFILE_PRIVATE)
    }

    fn judge(s: &Snapshot, applies: ScopeFn) -> FirewallStatus {
        evaluate_with(s, Path::new(EXE), Some(47300), applies)
    }

    #[test]
    fn a_block_rule_scoped_to_another_user_does_not_apply() {
        let s = with_allow(other_user_inbound(UserScope::Sddl(OTHER_USER_SDDL.into())));
        assert_eq!(judge(&s, &token_applies), FirewallStatus::Ok);
    }

    #[test]
    fn the_same_block_rule_scoped_to_our_sid_blocks_and_is_named() {
        let sddl = format!("O:LSD:(A;;CC;;;{})", my_sid());
        let s = with_allow(other_user_inbound(UserScope::Sddl(sddl)));
        let v = judge(&s, &token_applies);
        assert_eq!(v, FirewallStatus::BlockedByRule { rule: "sandbox_offline_block_inbound".into(), note: None });
        let msg = FirewallInfo::from_status(v).message.unwrap();
        assert!(msg.contains("sandbox_offline_block_inbound"), "{msg}");
    }

    #[test]
    fn an_alias_sid_the_user_belongs_to_makes_the_rule_apply() {
        // Everyone and Builtin Users hold for any normal user token; Anonymous never does.
        for alias in ["WD", "BU"] {
            let s = with_allow(other_user_inbound(UserScope::Sddl(format!("O:LSD:(A;;CC;;;{alias})"))));
            assert!(matches!(judge(&s, &token_applies), FirewallStatus::BlockedByRule { note: None, .. }), "{alias}");
        }
        let s = with_allow(other_user_inbound(UserScope::Sddl("O:LSD:(A;;CC;;;AN)".into())));
        assert_eq!(judge(&s, &token_applies), FirewallStatus::Ok);
    }

    #[test]
    fn a_deny_entry_for_our_sid_stops_the_rule_applying() {
        // Deny first: the token matches both entries and the first one decides.
        let sddl = format!("O:LSD:(D;;CC;;;{})(A;;CC;;;WD)", my_sid());
        let s = with_allow(other_user_inbound(UserScope::Sddl(sddl)));
        assert_eq!(judge(&s, &token_applies), FirewallStatus::Ok);
        // A deny for someone else changes nothing.
        let sddl = format!("O:LSD:(D;;CC;;;{OTHER_USER})(A;;CC;;;WD)");
        let s = with_allow(other_user_inbound(UserScope::Sddl(sddl)));
        assert!(matches!(judge(&s, &token_applies), FirewallStatus::BlockedByRule { .. }));
    }

    #[test]
    fn entries_without_the_match_right_are_not_what_decides() {
        // A deny that carries another right does not stop the allow after it: the rule applies.
        for sddl in ["O:LSD:(D;;0x2;;;WD)(A;;CC;;;WD)", "O:LSD:(D;;GA;;;WD)(A;;CC;;;WD)", "O:LSD:(D;;RC;;;AU)(A;;CC;;;AU)"] {
            let s = with_allow(other_user_inbound(UserScope::Sddl(sddl.into())));
            assert!(matches!(judge(&s, &token_applies), FirewallStatus::BlockedByRule { note: None, .. }), "{sddl}");
        }
        // An allow that carries another right grants nothing: that allow rule is not ours.
        let mine = RuleInfo { user_scope: UserScope::Sddl("O:LSD:(A;;0x2;;;WD)".into()), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert_eq!(judge(&snap(vec![mine], PROFILE_PRIVATE), &token_applies), FirewallStatus::RuleMissing);
    }

    #[test]
    fn an_unparseable_user_scope_fails_closed_with_the_reason() {
        let s = with_allow(other_user_inbound(UserScope::Sddl("this is not sddl".into())));
        match judge(&s, &token_applies) {
            FirewallStatus::BlockedByRule { rule, note: Some(why) } => {
                assert_eq!(rule, "sandbox_offline_block_inbound");
                assert!(why.contains("user scope"), "{why}");
            }
            other => panic!("expected a reasoned block, got {other:?}"),
        }
        // A scope the API would not give us is treated the same way.
        let s = with_allow(other_user_inbound(UserScope::Unreadable("cannot read the rule's user scope (x)".into())));
        assert!(matches!(judge(&s, &token_applies), FirewallStatus::BlockedByRule { note: Some(_), .. }));
        // So is an access check that itself fails.
        let s = with_allow(other_user_inbound(UserScope::Sddl(OTHER_USER_SDDL.into())));
        assert!(matches!(judge(&s, &|_| Err("x".into())), FirewallStatus::BlockedByRule { note: Some(_), .. }));
        // And a failed check never counts an allow rule.
        let mine = RuleInfo { user_scope: UserScope::Sddl(OTHER_USER_SDDL.into()), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        assert_eq!(judge(&snap(vec![mine], PROFILE_PRIVATE), &|_| Err("x".into())), FirewallStatus::RuleMissing);
    }

    #[test]
    fn an_allow_rule_scoped_to_another_user_is_not_our_allow() {
        let mine = RuleInfo { user_scope: UserScope::Sddl(OTHER_USER_SDDL.into()), ..rule(PROFILE_PRIVATE | PROFILE_PUBLIC) };
        let s = snap(vec![mine], PROFILE_PRIVATE);
        assert_eq!(judge(&s, &token_applies), FirewallStatus::RuleMissing);
    }

    #[test]
    fn a_loopback_only_remote_scope_does_not_block_the_phone() {
        for remote in ["127.0.0.0/255.0.0.0,::/127", "127.0.0.0/8", "127.0.0.1", "::1"] {
            let block = RuleInfo { remote_addresses: remote.into(), ..other_user_inbound(UserScope::All) };
            assert_eq!(judge(&with_allow(block), &token_applies), FirewallStatus::Ok, "{remote}");
        }
        // The same block with a LAN subnet in it does.
        let block = RuleInfo { remote_addresses: "127.0.0.0/8,192.168.0.0/255.255.0.0".into(), ..other_user_inbound(UserScope::All) };
        assert!(matches!(judge(&with_allow(block), &token_applies), FirewallStatus::BlockedByRule { .. }));
    }

    #[test]
    fn a_rule_for_a_package_a_service_or_another_program_does_not_apply() {
        for odd in [
            RuleInfo { package: "S-1-15-2-1-2-3-4-5-6-7".into(), ..other_user_inbound(UserScope::All) },
            RuleInfo { service: "Spooler".into(), ..other_user_inbound(UserScope::All) },
            RuleInfo { app: r"C:\other\app.exe".into(), ..other_user_inbound(UserScope::All) },
        ] {
            assert_eq!(judge(&with_allow(odd.clone()), &token_applies), FirewallStatus::Ok, "{odd:?}");
        }
        // Block bound to our own program does apply.
        let ours = RuleInfo { app: EXE.into(), ..other_user_inbound(UserScope::All) };
        assert!(matches!(judge(&with_allow(ours), &token_applies), FirewallStatus::BlockedByRule { .. }));
    }

    #[test]
    fn block_rules_on_other_profiles_or_interfaces_or_protocols_or_ports_do_not_apply() {
        for odd in [
            // Private is the active profile; the block is for Domain and Public only.
            RuleInfo { profiles: PROFILE_DOMAIN | PROFILE_PUBLIC, ..other_user_inbound(UserScope::All) },
            RuleInfo { interface_types: "RemoteAccess".into(), ..other_user_inbound(UserScope::All) },
            RuleInfo { protocol: 6, ..other_user_inbound(UserScope::All) },
            RuleInfo { local_ports: "80,443,5000-5100".into(), ..other_user_inbound(UserScope::All) },
            RuleInfo { enabled: false, ..other_user_inbound(UserScope::All) },
            RuleInfo { inbound: false, ..other_user_inbound(UserScope::All) },
        ] {
            assert_eq!(judge(&with_allow(odd.clone()), &token_applies), FirewallStatus::Ok, "{odd:?}");
        }
        // Our port inside a listed range, or a UDP-only block, does apply.
        for hit in [
            RuleInfo { local_ports: "47000-47999".into(), ..other_user_inbound(UserScope::All) },
            RuleInfo { protocol: PROTO_UDP, interface_types: "Lan,Wireless".into(), ..other_user_inbound(UserScope::All) },
            RuleInfo { profiles: PROFILE_PRIVATE, ..other_user_inbound(UserScope::All) },
        ] {
            assert!(matches!(judge(&with_allow(hit.clone()), &token_applies), FirewallStatus::BlockedByRule { .. }), "{hit:?}");
        }
    }

    #[test]
    fn a_block_beats_an_allow_when_it_applies() {
        let s = with_allow(other_user_inbound(UserScope::All));
        assert!(matches!(judge(&s, &token_applies), FirewallStatus::BlockedByRule { .. }));
    }

    /// The real Win32 parser and access check on the exact string Windows reports.
    #[test]
    fn the_real_user_scoped_rule_is_read_and_does_not_apply() {
        let scope = UserScope::Sddl(OTHER_USER_SDDL.into());
        // The injected check receives the rule's text untouched.
        let seen = std::cell::RefCell::new(Vec::new());
        let fit = user_fit(&scope, &|s| {
            seen.borrow_mut().push(s.to_string());
            Ok(false)
        });
        assert_eq!(fit, Fit::No);
        assert_eq!(*seen.borrow(), vec![OTHER_USER_SDDL.to_string()]);
        // The real check: a scope everybody is in applies, one nobody here is in does not.
        assert_eq!(user_fit(&scope, &token_applies), Fit::No);
        assert_eq!(user_fit(&UserScope::Sddl("O:LSD:(A;;CC;;;WD)".into()), &token_applies), Fit::Yes);
        assert_eq!(user_fit(&UserScope::Sddl(format!("O:LSD:(A;;CC;;;{})", my_sid())), &token_applies), Fit::Yes);
        // No group and no owner in the text still checks (a firewall scope has only a list).
        assert_eq!(user_fit(&UserScope::Sddl("D:(A;;CC;;;WD)".into()), &token_applies), Fit::Yes);
        // An empty list grants nobody.
        assert_eq!(user_fit(&UserScope::Sddl("O:LSD:".into()), &token_applies), Fit::No);
    }

    /// Live and read-only: the real firewall on this PC, judged for the installed program path.
    /// Run explicitly: `cargo test live_verdict -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads this PC's firewall; run explicitly"]
    fn live_verdict_for_the_installed_app() {
        let snap = snapshot().expect("snapshot");
        let exe = Path::new(EXE);
        let status = evaluate_with_port(&snap, exe, Some(47300));
        let info = FirewallInfo::from_status(status.clone());
        println!("current profiles bits: {}", snap.current_profiles);
        println!("rules read: {}", snap.rules.len());
        println!("verdict: {status:?}");
        println!("message: {:?} can_fix: {}", info.message, info.can_fix);
        println!("check() for the test binary: {:?}", check(Some(47300)));
        assert_eq!(status, FirewallStatus::Ok);
    }

    /// The real API on this machine: only checks that the call works and the answer is
    /// internally consistent (the rule is usually absent on a developer PC).
    #[test]
    fn the_real_firewall_api_answers() {
        let s = snapshot().expect("INetFwPolicy2 snapshot");
        assert!(s.current_profiles != 0, "some profile is active");
        assert_eq!(s.profiles.len(), 3);
        let info = check(Some(47300));
        assert!(!matches!(info.status, FirewallStatus::Unknown { .. }), "{info:?}");
    }
}
