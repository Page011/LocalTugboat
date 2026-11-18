//! Name sanitising and media type checks. Owner: logic builder. Pure.
//!
//! PLAN Security: "The receiver generates every on-disk name itself":
//! N1 NFC-normalise (unicode-normalization).
//! N2 Remove Unicode control chars (general category Cc) and bidi controls
//!    (U+061C, U+200E, U+200F, U+202A-U+202E, U+2066-U+2069).
//! N3 ':' -> NameError::Colon (NTFS alternate data streams). '/', '\\' -> PathSeparator.
//!    A name equal to "." or "..", or containing "..", -> DotDot. Also reject the other
//!    Windows-invalid chars < > " | ? * -> Invalid(char).
//! N4 Strip trailing dots and spaces (Windows drops them silently, which would make two
//!    names collide); trim leading spaces.
//! N5 Windows reserved device names -> Reserved: CON PRN AUX NUL COM0-COM9 LPT0-LPT9 and the
//!    superscript forms COM¹ COM² COM³ LPT¹ LPT² LPT³, case-insensitive, compared on the part
//!    before the first '.' after N4 (so "con.jpg" and "CON .jpg" are reserved too).
//! N6 Length cap: whole file name <= MAX_NAME_UTF16 UTF-16 code units; cut the stem at a
//!    char boundary, keep the extension.
//! N7 Photos and videos: the extension must be on the media list AND agree with the magic
//!    bytes (`sniff`), so a renamed program can never pose as a photo and get a thumbnail.
//!    Every other file is a plain file (`MediaKind::File`) with its own safe extension (or none).
//!    Media families and extensions:
//!      Jpeg: jpg jpeg   magic FF D8 FF
//!      Png:  png        magic 89 50 4E 47 0D 0A 1A 0A
//!      Gif:  gif        magic "GIF87a" | "GIF89a"
//!      Webp: webp       magic "RIFF" ???? "WEBP"
//!      Heif: heic heif  ISO-BMFF ("ftyp" at offset 4), major brand in
//!                       heic heix hevc hevx heim heis hevm hevs mif1 msf1
//!      Mp4:  mp4 m4v    ISO-BMFF, major brand in isom iso2 iso4 iso5 iso6 mp41 mp42 avc1 "M4V " dash mmp4 MSNV
//!      Mov:  mov        ISO-BMFF, major brand "qt  "
//!      ThreeGp: 3gp     ISO-BMFF, major brand starts with "3gp" or "3g2"
//!    Agreement: the extension's family equals the sniffed family, except that the video
//!    containers Mp4/Mov/ThreeGp accept each other (phones mislabel them; all are ISO-BMFF
//!    video). Extension compare is case-insensitive; the stored ext is lowercase.
//! N7b Always refused, both directions (R1, 0.2.0): .lnk .url .scf .library-ms
//!    .searchConnector-ms, desktop.ini, autorun.inf. Matched on the cleaned name (NFC, control
//!    and bidi characters out, trailing dots and spaces out, case-insensitive, an NTFS stream
//!    suffix ":..." ignored), on the record's extension and on the final disk name.
//! N8 Empty after cleaning -> Empty. The caller then uses `fallback_name`.
//! N9 Collisions on disk: `disambiguate` adds " (2)", " (3)", ... before the extension (the
//!    `ext_of` rule: up to 16 letters, digits, '-' or '_'), lowest free number; the record
//!    keeps the original name (PLAN Storage).
//! Folder names (`clean_folder_name`): N1-N6 without the extension rules, plus a leading '.'
//! is rejected (protects the hidden `.tugboat` marker folder and avoids hidden folders).

use unicode_normalization::UnicodeNormalization;

use crate::model::MediaKind;

/// Max whole-name length in UTF-16 units (NTFS allows 255; leave room for " (999)" and
/// for the PC's path). Folder names use the same cap.
pub const MAX_NAME_UTF16: usize = 150;

/// Bytes `sniff` needs from the start of the file.
pub const SNIFF_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("name is empty")]
    Empty,
    #[error("name contains ':'")]
    Colon,
    #[error("name contains a path separator")]
    PathSeparator,
    #[error("name contains '..'")]
    DotDot,
    #[error("name contains '{0}'")]
    Invalid(char),
    #[error("'{0}' is a reserved Windows name")]
    Reserved(String),
    #[error("folder name starts with '.'")]
    LeadingDot,
    /// A file extension that cannot be part of a path (not 1-16 of a-z 0-9 '-' '_').
    #[error("extension '{0}' cannot be used")]
    ExtensionNotAllowed(String),
    /// A photo or video extension whose content is not a photo or video.
    #[error("file content is not a photo or video")]
    NotMedia,
    #[error("extension '{ext}' does not match the file content ({detected:?})")]
    MagicMismatch { ext: String, detected: Family },
    /// One of the always-refused names or extensions (the payload is what was refused).
    #[error("REFUSED - shortcut and folder-setting files never sync")]
    Refused(String),
    /// A received record whose kind and extension disagree (a File with a photo extension, or a
    /// photo/video with another extension).
    #[error("the file kind does not match its extension '{0}'")]
    KindMismatch(String),
    /// A folder name ending in ".{GUID}": Explorer treats it as a shell junction (it shows the
    /// text before the dot and opens a system folder), which would hide the files inside.
    #[error("folder name ends in a shell class id")]
    ShellJunction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Family {
    Jpeg,
    Png,
    Gif,
    Webp,
    Heif,
    Mp4,
    Mov,
    ThreeGp,
}

impl Family {
    pub fn kind(self) -> MediaKind {
        match self {
            Family::Mp4 | Family::Mov | Family::ThreeGp => MediaKind::Video,
            _ => MediaKind::Image,
        }
    }

    /// Default extension used by `fallback_name`.
    pub fn default_ext(self) -> &'static str {
        match self {
            Family::Jpeg => "jpg",
            Family::Png => "png",
            Family::Gif => "gif",
            Family::Webp => "webp",
            Family::Heif => "heic",
            Family::Mp4 => "mp4",
            Family::Mov => "mov",
            Family::ThreeGp => "3gp",
        }
    }
}

fn is_bidi(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Family from the first SNIFF_LEN bytes (fewer bytes allowed: short input -> None unless
/// the magic fits). None = not an allowed photo/video.
pub fn sniff(head: &[u8]) -> Option<Family> {
    if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(Family::Jpeg);
    }
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(Family::Png);
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some(Family::Gif);
    }
    if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some(Family::Webp);
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let brand = &head[8..12];
        return match brand {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" | b"mif1" | b"msf1" => {
                Some(Family::Heif)
            }
            b"qt  " => Some(Family::Mov),
            b"isom" | b"iso2" | b"iso4" | b"iso5" | b"iso6" | b"mp41" | b"mp42" | b"avc1" | b"M4V " | b"dash"
            | b"mmp4" | b"MSNV" => Some(Family::Mp4),
            b if b.starts_with(b"3gp") || b.starts_with(b"3g2") => Some(Family::ThreeGp),
            _ => None,
        };
    }
    None
}

/// Family of an extension (case-insensitive, no dot). None = not on the allowlist.
pub fn ext_family(ext: &str) -> Option<Family> {
    match ext.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => Some(Family::Jpeg),
        "png" => Some(Family::Png),
        "gif" => Some(Family::Gif),
        "webp" => Some(Family::Webp),
        "heic" | "heif" => Some(Family::Heif),
        "mp4" | "m4v" => Some(Family::Mp4),
        "mov" => Some(Family::Mov),
        "3gp" => Some(Family::ThreeGp),
        _ => None,
    }
}

fn is_video(f: Family) -> bool {
    f.kind() == MediaKind::Video
}

/// N7: Ok(lowercase ext, sniffed family) if `ext` is allowed and agrees with `head`.
pub fn check_media(ext: &str, head: &[u8]) -> Result<(String, Family), NameError> {
    let ext_l = ext.to_ascii_lowercase();
    let from_ext = ext_family(&ext_l).ok_or_else(|| NameError::ExtensionNotAllowed(ext_l.clone()))?;
    let detected = sniff(head).ok_or(NameError::NotMedia)?;
    // Phones mislabel ISO-BMFF video containers; all three are the same kind of file.
    let agrees = from_ext == detected || (is_video(from_ext) && is_video(detected));
    if !agrees {
        return Err(NameError::MagicMismatch { ext: ext_l, detected });
    }
    Ok((ext_l, detected))
}

/// Longest extension `ext_of` / `is_safe_ext` accept.
pub const MAX_EXT_LEN: usize = 16;

/// What a refusal says, everywhere (activity row, banner, Not synced list).
pub const REFUSED_TEXT: &str = "REFUSED - shortcut and folder-setting files never sync";

/// Extensions that are always refused (lowercase, no dot).
const REFUSED_EXTS: [&str; 5] = ["lnk", "url", "scf", "library-ms", "searchconnector-ms"];
/// File names that are always refused (lowercase).
const REFUSED_NAMES: [&str; 2] = ["desktop.ini", "autorun.inf"];

/// Display-name preparation shared by `ext_of` and `is_refused_name`: NFC, control and bidi
/// characters out, trailing dots and spaces out (Windows drops them silently, so "x.lnk. " IS
/// "x.lnk" on disk). Order matters: `ext_of("a.pdf.")` must see "pdf".
pub fn prep(name: &str) -> String {
    let s: String = name.nfc().filter(|c| !c.is_control() && !is_bidi(*c)).collect();
    s.trim_end_matches(['.', ' ']).to_string()
}

fn ext_chars_ok(ext: &str) -> bool {
    (1..=MAX_EXT_LEN).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Extension of a file name: the text after the LAST '.' of the prepared name when that dot is
/// not its first character and the text is 1..=16 of `a-z 0-9 - _` (any case in, lowercase
/// out). Otherwise "" (no extension): "Makefile", ".gitignore", "x.weird ext".
pub fn ext_of(name: &str) -> String {
    let s = prep(name);
    match s.rfind('.') {
        Some(i) if i > 0 && ext_chars_ok(&s[i + 1..]) => s[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// An extension that may become part of a path: empty, or 1..=16 lowercase `a-z 0-9 - _`.
/// Used on every extension that comes from a record before it is used in a path. The one
/// exception to the length cap is an always-refused extension (`searchconnector-ms` has 18
/// characters): such a record must still be accepted as a record so both devices' digests stay
/// equal, then refused at download time (`is_refused_record`); it never reaches a path.
pub fn is_safe_ext(ext: &str) -> bool {
    ext.is_empty() || ((ext_chars_ok(ext) || REFUSED_EXTS.contains(&ext)) && ext == ext.to_ascii_lowercase())
}

/// The record's extension is one of the always-refused ones (a record can carry an extension
/// whose name is harmless: name "x", ext "lnk", which the disk name would join into "x.lnk").
pub fn is_refused_ext(ext: &str) -> bool {
    let e: String = ext.trim_start_matches('.').nfc().collect::<String>().to_lowercase();
    REFUSED_EXTS.contains(&e.as_str())
}

/// The always-refused names (module doc N7b). Checks the whole prepared name, its last path
/// segment, and both without an NTFS stream suffix (`x.lnk::$DATA` is the file `x.lnk`).
pub fn is_refused_name(name: &str) -> bool {
    let s = prep(name).to_lowercase();
    let segment = s.rsplit(['/', '\\']).next().unwrap_or("").to_string();
    let refused = |t: &str| {
        let t = t.trim_end_matches(['.', ' ']);
        REFUSED_NAMES.contains(&t) || REFUSED_EXTS.iter().any(|e| t.len() > e.len() && t.ends_with(&format!(".{e}")))
    };
    let mut found = false;
    for cand in [s.as_str(), segment.as_str()] {
        let main = cand.split(':').next().unwrap_or("");
        found |= refused(cand) || refused(main);
    }
    found
}

/// "stem" or "stem.ext".
pub fn join_ext(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        stem.to_string()
    } else {
        format!("{stem}.{ext}")
    }
}

/// Media family of a file NAME (by extension only). Some = make a thumbnail and apply the
/// photo/video content check; None = a plain file.
pub fn media_family_of_name(name: &str) -> Option<Family> {
    ext_family(&ext_of(name))
}

/// What a file with this extension and these first bytes is.
/// - refused extension -> `Refused`;
/// - photo/video extension -> the content must match (`check_media`), else `MagicMismatch` /
///   `NotMedia`: a renamed program never poses as a photo;
/// - any other safe extension, or none -> `(File, ext)`. No promotion: "" + JPEG bytes stays a
///   File named as it is (renaming the user's file is never ours to do).
pub fn classify(ext: &str, head: &[u8]) -> Result<(MediaKind, String), NameError> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    if is_refused_ext(&ext) {
        return Err(NameError::Refused(ext));
    }
    if ext_family(&ext).is_some() {
        let (e, fam) = check_media(&ext, head)?;
        return Ok((fam.kind(), e));
    }
    if !is_safe_ext(&ext) {
        return Err(NameError::ExtensionNotAllowed(ext));
    }
    Ok((MediaKind::File, ext))
}

/// Check a RECEIVED record's kind and extension against the bytes (never re-derives them):
/// photo/video -> a media extension of that kind whose content matches; File -> a safe,
/// non-media extension. Refused extensions are always an error.
pub fn verify_received(kind: MediaKind, ext: &str, head: &[u8]) -> Result<(), NameError> {
    if is_refused_ext(ext) {
        return Err(NameError::Refused(ext.to_string()));
    }
    match kind {
        MediaKind::File => {
            if ext_family(ext).is_some() {
                return Err(NameError::KindMismatch(ext.to_string()));
            }
            if !is_safe_ext(ext) {
                return Err(NameError::ExtensionNotAllowed(ext.to_string()));
            }
            Ok(())
        }
        MediaKind::Image | MediaKind::Video => {
            let fam = ext_family(ext).ok_or_else(|| NameError::KindMismatch(ext.to_string()))?;
            if fam.kind() != kind {
                return Err(NameError::KindMismatch(ext.to_string()));
            }
            check_media(ext, head).map(|_| ())
        }
    }
}

/// N1-N4, shared by file stems and folder names.
fn clean_common(raw: &str) -> Result<String, NameError> {
    let s: String = raw.nfc().filter(|c| !c.is_control() && !is_bidi(*c)).collect();
    for c in s.chars() {
        match c {
            ':' => return Err(NameError::Colon),
            '/' | '\\' => return Err(NameError::PathSeparator),
            '<' | '>' | '"' | '|' | '?' | '*' => return Err(NameError::Invalid(c)),
            _ => {}
        }
    }
    if s == "." || s.contains("..") {
        return Err(NameError::DotDot);
    }
    let s = s.trim_start_matches(' ').trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() {
        return Err(NameError::Empty);
    }
    Ok(s)
}

/// N5: Windows device names, compared on the part before the first '.'.
fn check_reserved(s: &str) -> Result<(), NameError> {
    let base = s.split('.').next().unwrap_or("").trim_end_matches(' ').to_uppercase();
    let numbered = ["COM", "LPT"].iter().any(|p| {
        base.strip_prefix(p).is_some_and(|n| {
            let mut it = n.chars();
            matches!((it.next(), it.next()), (Some('0'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}'), None))
        })
    });
    if matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL") || numbered {
        return Err(NameError::Reserved(base));
    }
    Ok(())
}

/// N6: keep at most `max` UTF-16 code units, cut on a char boundary, never leave a trailing
/// dot or space (Windows would drop it and the name would change).
fn cap_utf16(s: &str, max: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    out.trim_end_matches(['.', ' ']).to_string()
}

/// N1-N6, N8 for a file stem taken from a record. Extension rules are separate
/// (`check_media`), because the record's name may lack or misstate the extension: the disk
/// name is `<clean stem>.<checked ext>`.
pub fn clean_stem(raw_name: &str) -> Result<String, NameError> {
    let s = clean_common(raw_name)?;
    check_reserved(&s)?;
    let s = cap_utf16(&s, MAX_NAME_UTF16);
    if s.is_empty() {
        return Err(NameError::Empty);
    }
    // Capping and trimming can leave a device name ("CON" + spaces + "x" -> "CON"): check the result.
    check_reserved(&s)?;
    Ok(s)
}

/// The record name without a trailing ".<ext>" (case-insensitive), if it has one.
pub fn strip_ext<'a>(name: &'a str, ext: &str) -> &'a str {
    let ext = ext.trim_start_matches('.');
    if ext.is_empty() || name.len() <= ext.len() {
        return name;
    }
    let dot = name.len() - ext.len() - 1;
    match name.get(dot..) {
        Some(tail) if tail.starts_with('.') && tail[1..].eq_ignore_ascii_case(ext) => &name[..dot],
        _ => name,
    }
}

/// The on-disk file name for a received item: the cleaned record name without its extension,
/// plus "." and the checked extension (no dot when the extension is empty). A name that does not
/// clean (empty, reserved, a colon, ...) falls back to `fallback_stem(captured_at)`: the file is
/// still accepted, and only type and refusal failures refuse it (decision B: the made-up name keeps
/// today's `IMG_` look whatever the kind). Returns (name, kind). A final name that is one of the
/// always-refused ones (`desktop` + extension `ini`) is `Refused` too.
pub fn disk_file_name(record_name: &str, ext: &str, head: &[u8], captured_at_ms: i64) -> Result<(String, MediaKind), NameError> {
    // Type first: a type failure refuses the file whatever the name looks like.
    let (kind, ext) = classify(ext, head)?;
    let mut name = build_disk_name(record_name, &ext, captured_at_ms);
    // A plain file never lands under a photo/video name (Explorer would thumbnail and open it as a
    // photo): a File record without an extension whose name ends in one gets the made-up name.
    if kind == MediaKind::File && ext.is_empty() && media_family_of_name(&name).is_some() {
        name = fallback_stem(captured_at_ms);
    }
    if is_refused_name(&name) {
        return Err(NameError::Refused(name));
    }
    Ok((name, kind))
}

/// The cleaned "stem.ext" `disk_file_name` builds (no type check, no refusal check).
fn build_disk_name(record_name: &str, ext: &str, captured_at_ms: i64) -> String {
    let room = MAX_NAME_UTF16.saturating_sub(if ext.is_empty() { 0 } else { 1 + ext.len() });
    let stem = match clean_stem(strip_ext(record_name, ext)) {
        Ok(s) => {
            let capped = cap_utf16(&s, room);
            // The smaller room can cut a name down to a device name too.
            if capped.is_empty() || check_reserved(&capped).is_err() {
                fallback_stem(captured_at_ms)
            } else {
                capped
            }
        }
        Err(_) => fallback_stem(captured_at_ms),
    };
    join_ext(&stem, ext)
}

/// A synced record that must never be downloaded: its extension, its name, or the disk name this
/// device would build from them is one of the always-refused ones (amendment 1: name "a:b" with
/// ext "lnk" would otherwise become `IMG_<time>.lnk`). Pure; needs no file bytes.
pub fn is_refused_record(record_name: &str, ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    is_refused_ext(&ext) || is_refused_name(record_name) || is_refused_name(&build_disk_name(record_name, &ext, 0))
}

/// Folder-name rules (see module doc).
pub fn clean_folder_name(raw: &str) -> Result<String, NameError> {
    let s = clean_common(raw)?;
    if s.starts_with('.') {
        return Err(NameError::LeadingDot);
    }
    if is_shell_junction(&s) {
        return Err(NameError::ShellJunction);
    }
    check_reserved(&s)?;
    let s = cap_utf16(&s, MAX_NAME_UTF16);
    if s.is_empty() {
        return Err(NameError::Empty);
    }
    // Capping and trimming can leave a device name ("CON" + spaces + "x" -> "CON"): check the result.
    check_reserved(&s)?;
    Ok(s)
}

/// "Name.{20D04FE0-3AEA-1069-A2D8-08002B30309D}": the text after the last '.' is a braced GUID.
fn is_shell_junction(s: &str) -> bool {
    let Some(i) = s.rfind('.') else { return false };
    let g = &s.as_bytes()[i + 1..];
    g.len() == 38
        && g[0] == b'{'
        && g[37] == b'}'
        && g[1..37].iter().enumerate().all(|(n, b)| if matches!(n, 8 | 13 | 18 | 23) { *b == b'-' } else { b.is_ascii_hexdigit() })
}

/// Control or bidi characters in a name (never in a name this device built).
pub fn has_hidden_chars(name: &str) -> bool {
    name.chars().any(|c| c.is_control() || is_bidi(c))
}

/// The name to SHOW for a record that came from the other device (a hostile or broken peer can send
/// anything; the record itself stays as received so both devices' digests stay equal). Control and
/// direction characters are out ("invoice<RLO>fdp.exe" would read "invoiceexe.pdf"), and the name
/// ends in ".<ext>" when the record has an extension, so it cannot hide the real type ("invoice.pdf"
/// with ext "exe" shows "invoice.pdf.exe", the name of the file on disk). Honest names (`stem.ext`)
/// come back unchanged.
pub fn display_name(name: &str, ext: &str) -> String {
    let s: String = if has_hidden_chars(name) { name.chars().filter(|c| !c.is_control() && !is_bidi(*c)).collect() } else { name.to_string() };
    if ext.is_empty() || strip_ext(&s, ext).len() < s.len() {
        s
    } else {
        format!("{s}.{ext}")
    }
}

/// A plain file without an extension may not be named like a photo/video (it would be placed under
/// that name and open as a photo).
pub fn plain_name_ok(name: &str, ext: &str) -> bool {
    !ext.is_empty() || media_family_of_name(name).is_none()
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian (Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// "IMG_20261117_140233" (no extension).
pub fn fallback_stem(captured_at_ms: i64) -> String {
    let secs = captured_at_ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!("IMG_{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// "IMG_20261117_140233.jpg" from the capture time (UTC) and family.
pub fn fallback_name(captured_at_ms: i64, family: Family) -> String {
    format!("{}.{}", fallback_stem(captured_at_ms), family.default_ext())
}

/// N9. `taken` answers case-insensitively (Windows); "a.jpg" -> "a (2).jpg" -> "a (3).jpg".
/// Names without an extension get the suffix at the end ("Work (2)").
pub fn disambiguate(name: &str, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    // The `ext_of` rule: a trailing ".xyz" of 1-16 letters, digits, '-' or '_' is the extension.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && ext_chars_ok(&name[i + 1..]) => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let mut candidate = name.to_string();
    for n in 2..=100_000u32 {
        candidate = format!("{stem} ({n}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    // The caller's collision check keeps saying "taken": hand back the last candidate and let
    // the filesystem report the real error rather than loop forever.
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.extend_from_slice(&[0; 28]);
        v
    }

    fn bmff(brand: &[u8; 4]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 0x18];
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(brand);
        v.extend_from_slice(&[0; 20]);
        v
    }

    fn riff_webp() -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[1, 2, 3, 4]);
        v.extend_from_slice(b"WEBP");
        v.extend_from_slice(&[0; 20]);
        v
    }

    fn png() -> Vec<u8> {
        let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        v.extend_from_slice(&[0; 24]);
        v
    }

    fn exe() -> Vec<u8> {
        let mut v = b"MZ".to_vec();
        v.extend_from_slice(&[0x90, 0, 3, 0, 0, 0, 4, 0, 0, 0, 0xFF, 0xFF, 0, 0]);
        v
    }

    // L23: every family accepted with a matching extension.
    #[test]
    fn every_family_good() {
        let cases: Vec<(&str, Vec<u8>, Family)> = vec![
            ("jpg", jpeg(), Family::Jpeg),
            ("JPEG", jpeg(), Family::Jpeg),
            ("png", png(), Family::Png),
            ("gif", b"GIF89a....".to_vec(), Family::Gif),
            ("gif", b"GIF87a....".to_vec(), Family::Gif),
            ("webp", riff_webp(), Family::Webp),
            ("heic", bmff(b"heic"), Family::Heif),
            ("heif", bmff(b"mif1"), Family::Heif),
            ("mp4", bmff(b"isom"), Family::Mp4),
            ("m4v", bmff(b"M4V "), Family::Mp4),
            ("mov", bmff(b"qt  "), Family::Mov),
            ("3gp", bmff(b"3gp4"), Family::ThreeGp),
            ("3gp", bmff(b"3g2a"), Family::ThreeGp),
        ];
        for (ext, head, fam) in cases {
            let (e, f) = check_media(ext, &head).unwrap_or_else(|e| panic!("{ext}: {e}"));
            assert_eq!(f, fam, "{ext}");
            assert_eq!(e, ext.to_ascii_lowercase());
        }
        for brand in [b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1"] {
            assert_eq!(sniff(&bmff(brand)), Some(Family::Heif), "{:?}", std::str::from_utf8(brand));
        }
        for brand in [b"isom", b"iso2", b"iso4", b"iso5", b"iso6", b"mp41", b"mp42", b"avc1", b"M4V ", b"dash", b"mmp4", b"MSNV"] {
            assert_eq!(sniff(&bmff(brand)), Some(Family::Mp4), "{:?}", std::str::from_utf8(brand));
        }
    }

    // L23: .exe with JPEG magic, .jpg with PNG magic, .mp4 with qt brand, desktop.ini, .lnk.
    #[test]
    fn extension_and_magic_must_agree() {
        assert!(matches!(check_media("exe", &jpeg()), Err(NameError::ExtensionNotAllowed(_))));
        assert!(matches!(check_media("ini", &jpeg()), Err(NameError::ExtensionNotAllowed(_))));
        assert!(matches!(check_media("lnk", &jpeg()), Err(NameError::ExtensionNotAllowed(_))));
        assert!(matches!(check_media("", &jpeg()), Err(NameError::ExtensionNotAllowed(_))));
        assert!(matches!(check_media("jpg", &png()), Err(NameError::MagicMismatch { detected: Family::Png, .. })));
        assert!(matches!(check_media("png", &jpeg()), Err(NameError::MagicMismatch { detected: Family::Jpeg, .. })));
        assert!(matches!(check_media("heic", &bmff(b"isom")), Err(NameError::MagicMismatch { .. })), "video brand is not an image");
        assert!(matches!(check_media("mp4", &png()), Err(NameError::MagicMismatch { .. })));
        // Executable content with a media extension.
        assert!(matches!(check_media("jpg", &exe()), Err(NameError::NotMedia)));
        assert!(matches!(check_media("jpg", &[]), Err(NameError::NotMedia)));
        assert!(matches!(check_media("jpg", &[0xFF, 0xD8]), Err(NameError::NotMedia)), "short input");
        // ISO-BMFF with an unknown brand.
        assert!(matches!(check_media("mp4", &bmff(b"zzzz")), Err(NameError::NotMedia)));
        // Video containers accept each other's extension.
        assert!(check_media("mp4", &bmff(b"qt  ")).is_ok());
        assert!(check_media("mov", &bmff(b"isom")).is_ok());
        assert!(check_media("3gp", &bmff(b"mp42")).is_ok());
        // RIFF that is not WebP (a WAV).
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0, 0, 0, 0]);
        wav.extend_from_slice(b"WAVE");
        assert_eq!(sniff(&wav), None);
    }

    #[test]
    fn exe_renamed_with_bidi_trick_is_refused_by_extension() {
        // "photo" + RLO + "gpj.exe" renders as "photoexe.jpg" in Explorer.
        let name = "photo\u{202E}gpj.exe";
        let cleaned = clean_stem(name).unwrap();
        assert!(!cleaned.contains('\u{202E}'));
        // An exe is a plain file now (any file type syncs); it never gets a photo kind or a thumbnail.
        assert_eq!(disk_file_name(name, "exe", &jpeg(), 0).unwrap().1, MediaKind::File);
        // If the record claims jpg but the bytes are an exe: not media.
        assert!(matches!(disk_file_name(name, "jpg", &exe(), 0), Err(NameError::NotMedia)));
    }

    // L23: NFC, ':', separators, '..', trailing dots and spaces.
    #[test]
    fn stem_cleaning_rules() {
        // NFC: e + combining acute -> precomposed.
        assert_eq!(clean_stem("cafe\u{0301}").unwrap(), "caf\u{00E9}");
        // Control characters are stripped (NUL, tab, newline, DEL, C1).
        assert_eq!(clean_stem("a\0b\tc\nd\u{7f}e\u{85}f").unwrap(), "abcdef");
        // Bidi controls are stripped.
        assert_eq!(clean_stem("a\u{202E}b\u{2066}c\u{200F}d\u{061C}e").unwrap(), "abcde");
        assert_eq!(clean_stem("IMG:1"), Err(NameError::Colon));
        assert_eq!(clean_stem("IMG.jpg:Zone.Identifier"), Err(NameError::Colon));
        assert_eq!(clean_stem("a/b"), Err(NameError::PathSeparator));
        assert_eq!(clean_stem("a\\b"), Err(NameError::PathSeparator));
        assert_eq!(clean_stem(".."), Err(NameError::DotDot));
        assert_eq!(clean_stem("."), Err(NameError::DotDot));
        assert_eq!(clean_stem("a..b"), Err(NameError::DotDot));
        assert_eq!(clean_stem("a.\u{202E}.b"), Err(NameError::DotDot), "bidi stripped before the '..' check");
        for c in ['<', '>', '"', '|', '?', '*'] {
            assert_eq!(clean_stem(&format!("a{c}b")), Err(NameError::Invalid(c)));
        }
        assert_eq!(clean_stem("IMG_1. . "), Ok("IMG_1".to_string()));
        assert_eq!(clean_stem("   lead"), Ok("lead".to_string()));
        assert_eq!(clean_stem(""), Err(NameError::Empty));
        assert_eq!(clean_stem("   "), Err(NameError::Empty));
        assert_eq!(clean_stem("\u{202E}\0"), Err(NameError::Empty));
    }

    #[test]
    fn reserved_windows_names() {
        for n in [
            "CON", "con", "Prn", "AUX", "NUL", "COM1", "com9", "COM0", "LPT1", "LPT9", "LPT\u{00B9}", "COM\u{00B2}", "com\u{00B3}",
            "con.jpg", "NUL .jpg", "con.tar.gz",
        ] {
            assert!(matches!(clean_stem(n), Err(NameError::Reserved(_))), "{n:?} should be reserved");
        }
        for n in ["CONSOLE", "COM10", "COMM", "LPT", "COM", "ICON", "a.con", "nul2"] {
            assert!(clean_stem(n).is_ok(), "{n:?} is fine");
        }
        // A reserved record name does not refuse the file: the disk name falls back.
        let (name, fam) = disk_file_name("CON", "jpg", &jpeg(), 1_794_000_000_000).unwrap();
        assert!(name.starts_with("IMG_") && name.ends_with(".jpg"), "{name}");
        assert_eq!(fam, MediaKind::Image);
        let (name, _) = disk_file_name("NUL .jpg", "jpg", &jpeg(), 1_794_000_000_000).unwrap();
        assert!(name.starts_with("IMG_"), "{name}");
        let (name, _) = disk_file_name("a:b.jpg", "jpg", &jpeg(), 1_794_000_000_000).unwrap();
        assert!(name.starts_with("IMG_"), "colon falls back: {name}");
    }

    // Truncation must not turn an allowed name into a device name.
    #[test]
    fn reserved_name_left_by_truncation_is_refused() {
        let long = format!("CON{}x", " ".repeat(147));
        assert!(matches!(clean_stem(&long), Err(NameError::Reserved(_))));
        assert!(matches!(clean_folder_name(&long), Err(NameError::Reserved(_))));
        // 149 units: passes the 150 cap, but disk_file_name's room for ".jpg" cuts it down to "CON".
        let mid = format!("CON{}x", " ".repeat(145));
        assert!(clean_stem(&mid).is_ok());
        let (name, _) = disk_file_name(&mid, "jpg", &jpeg(), 1_794_000_000_000).unwrap();
        assert!(name.starts_with("IMG_") && name.ends_with(".jpg"), "{name}");
    }

    // L23: 150-unit cap keeps the extension.
    #[test]
    fn length_cap_keeps_extension() {
        let long = "x".repeat(300);
        let (name, _) = disk_file_name(&format!("{long}.jpg"), "jpg", &jpeg(), 0).unwrap();
        assert_eq!(name.encode_utf16().count(), MAX_NAME_UTF16);
        assert!(name.ends_with(".jpg"));
        // Astral characters count as two UTF-16 units and are never cut in half.
        let emoji = "\u{1F4F7}".repeat(100);
        let (name, _) = disk_file_name(&format!("{emoji}.jpg"), "jpg", &jpeg(), 0).unwrap();
        assert!(name.encode_utf16().count() <= MAX_NAME_UTF16, "{}", name.encode_utf16().count());
        assert!(name.ends_with(".jpg"));
        // A cut that would leave a trailing dot or space is trimmed again.
        let tricky = format!("{}. {}", "y".repeat(149), "z".repeat(20));
        let cleaned = clean_stem(&tricky).unwrap();
        assert!(!cleaned.ends_with(' ') && !cleaned.ends_with('.'), "{cleaned:?}");
        assert!(cleaned.encode_utf16().count() <= MAX_NAME_UTF16);
        assert_eq!(clean_stem(&"q".repeat(1000)).unwrap().len(), MAX_NAME_UTF16);
    }

    #[test]
    fn disk_name_keeps_good_names_and_strips_the_extension_once() {
        let (n, _) = disk_file_name("Site photo.JPG", "jpg", &jpeg(), 0).unwrap();
        assert_eq!(n, "Site photo.jpg");
        let (n, _) = disk_file_name("Site photo", "jpg", &jpeg(), 0).unwrap();
        assert_eq!(n, "Site photo.jpg");
        let (n, _) = disk_file_name("a.jpg.jpg", "jpg", &jpeg(), 0).unwrap();
        assert_eq!(n, "a.jpg.jpg");
        let (n, _) = disk_file_name("", "mp4", &bmff(b"isom"), 0).unwrap();
        assert!(n.starts_with("IMG_") && n.ends_with(".mp4"));
        assert_eq!(strip_ext("a.jpg", "jpg"), "a");
        assert_eq!(strip_ext("jpg", "jpg"), "jpg");
        assert_eq!(strip_ext("\u{e9}\u{e9}", "jpg"), "\u{e9}\u{e9}", "multibyte names do not panic");
        assert_eq!(strip_ext("a\u{e9}jpg", "jpg"), "a\u{e9}jpg");
    }

    #[test]
    fn fallback_format() {
        // 2026-11-17 14:02:33 UTC
        let ms = 1_794_924_153_000;
        assert_eq!(fallback_name(ms, Family::Jpeg), "IMG_20261117_140233.jpg");
        assert_eq!(fallback_name(ms, Family::Mov), "IMG_20261117_140233.mov");
        assert_eq!(fallback_name(ms, Family::Heif), "IMG_20261117_140233.heic");
        assert_eq!(fallback_name(0, Family::Png), "IMG_19700101_000000.png");
        // Leap day and negative times.
        assert_eq!(fallback_stem(1_709_208_000_000), "IMG_20240229_120000");
        assert_eq!(fallback_stem(-1000), "IMG_19691231_235959");
    }

    // L23: disambiguate (2)(3) and no-ext, case-insensitive.
    #[test]
    fn disambiguate_lowest_free_number() {
        let taken_set = |set: &'static [&'static str]| move |n: &str| set.iter().any(|t| t.eq_ignore_ascii_case(n));
        assert_eq!(disambiguate("a.jpg", &taken_set(&[])), "a.jpg");
        assert_eq!(disambiguate("a.jpg", &taken_set(&["a.jpg"])), "a (2).jpg");
        assert_eq!(disambiguate("a.jpg", &taken_set(&["A.JPG", "a (2).jpg"])), "a (3).jpg");
        assert_eq!(disambiguate("a.jpg", &taken_set(&["a.jpg", "a (3).jpg"])), "a (2).jpg", "lowest free, not next");
        assert_eq!(disambiguate("Work", &taken_set(&["work"])), "Work (2)");
        assert_eq!(disambiguate("Work", &taken_set(&["Work", "Work (2)", "Work (3)"])), "Work (4)");
        assert_eq!(disambiguate("my.photo.png", &taken_set(&["my.photo.png"])), "my.photo (2).png");
        // Pathological checker: terminates.
        let always = |_: &str| true;
        assert!(disambiguate("a.jpg", &always).ends_with(").jpg"));
    }

    // L23: folder names, including ".tugboat".
    #[test]
    fn folder_names() {
        assert_eq!(clean_folder_name("Work"), Ok("Work".to_string()));
        assert_eq!(clean_folder_name(".tugboat"), Err(NameError::LeadingDot));
        assert_eq!(clean_folder_name(".hidden"), Err(NameError::LeadingDot));
        assert_eq!(clean_folder_name("a:b"), Err(NameError::Colon));
        assert_eq!(clean_folder_name("a/b"), Err(NameError::PathSeparator));
        assert_eq!(clean_folder_name(".."), Err(NameError::DotDot));
        assert!(matches!(clean_folder_name("CON"), Err(NameError::Reserved(_))));
        assert!(matches!(clean_folder_name("lpt1"), Err(NameError::Reserved(_))));
        assert_eq!(clean_folder_name("  Site 4. "), Ok("Site 4".to_string()));
        assert_eq!(clean_folder_name(""), Err(NameError::Empty));
        assert_eq!(clean_folder_name(&"f".repeat(400)).unwrap().len(), MAX_NAME_UTF16);
        // Folder names keep dots inside (no extension rules).
        assert_eq!(clean_folder_name("v1.2 report"), Ok("v1.2 report".to_string()));
    }

    // ---------- 0.2.0: any file type ----------

    #[test]
    fn ext_of_table() {
        for (name, want) in [
            ("a.JPG", "jpg"),
            ("archive.tar.gz", "gz"),
            (".gitignore", ""),
            ("Makefile", ""),
            ("x.weird ext", ""),
            ("y.library-ms", "library-ms"),
            ("a.pdf.", "pdf"),
            ("a.pdf. . ", "pdf"),
            ("a.", ""),
            ("a..b", "b"),
            ("data.sqlite3", "sqlite3"),
            ("x.tar-gz", "tar-gz"),
            ("a.\u{e9}t\u{e9}", ""),
            ("a.\u{202E}pdf", "pdf"),
            ("report.PDF", "pdf"),
        ] {
            assert_eq!(ext_of(name), want, "{name:?}");
        }
        assert_eq!(ext_of(&format!("a.{}", "x".repeat(16))), "x".repeat(16));
        assert_eq!(ext_of(&format!("a.{}", "x".repeat(17))), "", "17 characters is not an extension");
    }

    #[test]
    fn safe_ext_rule() {
        for ok in ["", "jpg", "pdf", "tar-gz", "a_b", "sqlite3"] {
            assert!(is_safe_ext(ok), "{ok:?}");
        }
        for bad in ["PDF", "pd f", "../x", "a.b", "x:y", "a/b", "\u{e9}", &"x".repeat(17)] {
            assert!(!is_safe_ext(bad), "{bad:?}");
        }
        // The one exception to the length cap: a refused extension still counts as a record's extension.
        assert!(is_safe_ext("searchconnector-ms"));
        assert!(!is_safe_ext("searchconnector-msx"));
    }

    // Amendment 1: a record is refused by its extension, its name or the disk name it would get.
    #[test]
    fn refused_records() {
        for (name, ext) in [
            ("a:b", "lnk"),
            ("x.lnk::$DATA", ""),
            ("x", "lnk"),
            ("desktop", "ini"),
            ("Desktop.Ini.", ""),
            ("x.ln\u{212A}", ""),
            ("a:b", "LNK"),
            ("z", "searchconnector-ms"),
            ("autorun", "inf"),
        ] {
            assert!(is_refused_record(name, ext), "{name:?} + {ext:?}");
        }
        for (name, ext) in [("report.pdf", "pdf"), ("notes", "ini"), ("a:b", "pdf"), ("Makefile", ""), ("lnk", "txt"), ("photo.jpg", "jpg")] {
            assert!(!is_refused_record(name, ext), "{name:?} + {ext:?}");
        }
    }

    #[test]
    fn refused_names_table() {
        for n in [
            "x.lnk", "X.LNK", "x.LnK. ", "x.lnk...", "a.url", "s.scf", "y.library-ms", "z.searchConnector-ms", "z.SEARCHCONNECTOR-MS",
            "desktop.ini", "DESKTOP.INI", "Desktop.Ini.", "autorun.inf", "autorun.inf ", "AUTORUN.INF. ", "dir/desktop.ini", "dir\\x.lnk",
            "x.lnk::$DATA", "desktop.ini:stream", "x.ln\u{212A}", "x.l\u{202E}nk", ".lnk",
        ] {
            assert!(is_refused_name(n), "{n:?} must be refused");
        }
        for n in ["lnk.txt", "desktop.ini.txt", "mylnk", "photo.jpg", "my desktop.ini.bak", "autorun.info", "x.lnk.txt", "notes.url.pdf", ""] {
            assert!(!is_refused_name(n), "{n:?} is fine");
        }
        for e in ["lnk", "LNK", ".url", "scf", "library-ms", "searchconnector-ms"] {
            assert!(is_refused_ext(e), "{e:?}");
        }
        for e in ["", "ini", "inf", "txt", "exe"] {
            assert!(!is_refused_ext(e), "{e:?}");
        }
    }

    #[test]
    fn classify_table() {
        for e in ["pdf", "txt", "zip", "docx", "exe", ""] {
            assert_eq!(classify(e, b"anything").unwrap(), (MediaKind::File, e.to_string()), "{e:?}");
        }
        assert_eq!(classify("PDF", b"x").unwrap().1, "pdf");
        assert_eq!(classify("jpg", &jpeg()).unwrap(), (MediaKind::Image, "jpg".into()));
        assert_eq!(classify("JPEG", &jpeg()).unwrap(), (MediaKind::Image, "jpeg".into()));
        assert_eq!(classify("mp4", &bmff(b"qt  ")).unwrap(), (MediaKind::Video, "mp4".into()));
        assert!(matches!(classify("jpg", &exe()), Err(NameError::NotMedia)), "a program posing as a photo");
        assert!(matches!(classify("png", &jpeg()), Err(NameError::MagicMismatch { .. })));
        // No promotion: no extension + JPEG bytes stays a plain file.
        assert_eq!(classify("", &jpeg()).unwrap(), (MediaKind::File, String::new()));
        for e in ["lnk", "URL", "scf", "library-ms", "searchConnector-ms"] {
            assert!(matches!(classify(e, b"x"), Err(NameError::Refused(_))), "{e:?}");
        }
        assert!(matches!(classify("a b", b"x"), Err(NameError::ExtensionNotAllowed(_))));
    }

    #[test]
    fn verify_received_never_rederives_kind_or_ext() {
        // Honest records.
        assert!(verify_received(MediaKind::Image, "jpg", &jpeg()).is_ok());
        assert!(verify_received(MediaKind::Video, "mp4", &bmff(b"isom")).is_ok());
        assert!(verify_received(MediaKind::File, "pdf", b"%PDF").is_ok());
        assert!(verify_received(MediaKind::File, "", b"x").is_ok());
        // A File that claims a photo extension would skip the magic check: refused.
        assert!(matches!(verify_received(MediaKind::File, "jpg", &exe()), Err(NameError::KindMismatch(_))));
        // A photo/video with another extension, or the wrong media kind.
        assert!(matches!(verify_received(MediaKind::Image, "pdf", &jpeg()), Err(NameError::KindMismatch(_))));
        assert!(matches!(verify_received(MediaKind::Image, "", &jpeg()), Err(NameError::KindMismatch(_))), "no promotion of an empty extension");
        assert!(matches!(verify_received(MediaKind::Video, "jpg", &jpeg()), Err(NameError::KindMismatch(_))));
        // Content still has to match.
        assert!(matches!(verify_received(MediaKind::Image, "jpg", &exe()), Err(NameError::NotMedia)));
        // Refused extensions never pass, whatever the kind.
        assert!(matches!(verify_received(MediaKind::File, "lnk", b"x"), Err(NameError::Refused(_))));
    }

    #[test]
    fn disk_names_for_any_file() {
        let t = 1_794_000_000_000;
        assert_eq!(disk_file_name("report.pdf", "pdf", b"%PDF", t).unwrap(), ("report.pdf".to_string(), MediaKind::File));
        assert_eq!(disk_file_name("Makefile", "", b"x", t).unwrap(), ("Makefile".to_string(), MediaKind::File));
        assert_eq!(disk_file_name("data", "csv", b"a,b", t).unwrap().0, "data.csv");
        // Decision B: an unsafe name still gets the made-up IMG_ name, whatever the kind.
        let (n, k) = disk_file_name("a:b.pdf", "pdf", b"%PDF", t).unwrap();
        assert!(n.starts_with("IMG_") && n.ends_with(".pdf"), "{n}");
        assert_eq!(k, MediaKind::File);
        let (n, _) = disk_file_name("a:b", "", b"x", t).unwrap();
        assert!(n.starts_with("IMG_") && !n.contains('.'), "{n}");
        // A final name that is refused is refused, even when the record name and ext are not.
        assert!(matches!(disk_file_name("desktop", "ini", b"x", t), Err(NameError::Refused(_))));
        assert!(matches!(disk_file_name("Desktop.Ini.", "", b"x", t), Err(NameError::Refused(_))));
        assert!(matches!(disk_file_name("x", "lnk", b"x", t), Err(NameError::Refused(_))));
        assert!(matches!(disk_file_name("a:b", "lnk", b"x", t), Err(NameError::Refused(_))), "the fallback name must not become IMG_<time>.lnk");
        assert!(disk_file_name("x.lnk::$DATA", "", b"x", t).is_ok(), "falls back to a made-up name: no stream, no .lnk");
        assert_eq!(join_ext("a", ""), "a");
        assert_eq!(join_ext("a", "b"), "a.b");
        assert_eq!(media_family_of_name("a.JPG"), Some(Family::Jpeg));
        assert_eq!(media_family_of_name("a.pdf"), None);
        assert_eq!(media_family_of_name("Makefile"), None);
    }

    #[test]
    fn disambiguate_uses_the_ext_of_rule() {
        let taken = |n: &str| n == "data.sqlite3" || n == "x.tar-gz" || n == "a.b_c";
        assert_eq!(disambiguate("data.sqlite3", &taken), "data (2).sqlite3");
        assert_eq!(disambiguate("x.tar-gz", &taken), "x (2).tar-gz");
        assert_eq!(disambiguate("a.b_c", &taken), "a (2).b_c");
        let long = format!("n.{}", "e".repeat(16));
        let l2 = long.clone();
        assert_eq!(disambiguate(&long, &move |n| n == l2), format!("n (2).{}", "e".repeat(16)));
    }

    #[test]
    fn a_folder_name_cannot_be_a_shell_junction() {
        for n in ["Photos.{20D04FE0-3AEA-1069-A2D8-08002B30309D}", "x.{645ff040-5081-101b-9f08-00aa002f954e}"] {
            assert_eq!(clean_folder_name(n), Err(NameError::ShellJunction), "{n}");
        }
        // Near misses stay fine.
        for n in ["Photos {20D04FE0-3AEA-1069-A2D8-08002B30309D}", "x.{20D04FE0-3AEA-1069-A2D8-08002B30309}", "x.{ZZD04FE0-3AEA-1069-A2D8-08002B30309D}", "v1.2"] {
            assert!(clean_folder_name(n).is_ok(), "{n}");
        }
    }

    #[test]
    fn display_name_cannot_hide_the_type_or_reverse_the_text() {
        assert_eq!(display_name("report.pdf", "pdf"), "report.pdf");
        assert_eq!(display_name("REPORT.PDF", "pdf"), "REPORT.PDF");
        assert_eq!(display_name("Makefile", ""), "Makefile");
        assert_eq!(display_name("invoice.pdf", "exe"), "invoice.pdf.exe");
        assert_eq!(display_name("invoice\u{202E}fdp.exe", "exe"), "invoicefdp.exe");
        assert_eq!(display_name("a\u{0}b\n", ""), "ab");
        assert_eq!(display_name("x", "tar-gz"), "x.tar-gz");
    }

    #[test]
    fn a_plain_file_never_lands_under_a_media_name() {
        assert!(!plain_name_ok("holiday.jpg", ""));
        assert!(!plain_name_ok("clip.MP4", ""));
        assert!(plain_name_ok("notes", ""));
        assert!(plain_name_ok("Dr. Smith letter", ""));
        assert!(plain_name_ok("holiday.jpg", "jpg"));
        let t = 1_790_000_000_000;
        for n in ["holiday.jpg", "clip.mp4", "scan.png"] {
            let (disk, kind) = disk_file_name(n, "", b"MZ plain bytes", t).unwrap();
            assert_eq!(kind, MediaKind::File);
            assert!(media_family_of_name(&disk).is_none(), "{n} -> {disk}");
        }
    }
}
