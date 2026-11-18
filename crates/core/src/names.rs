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
//! N7 Extension must be on the allowlist AND agree with the magic bytes (`sniff`), so a
//!    compromised phone cannot drop .exe, .lnk or desktop.ini into the library.
//!    Families and extensions:
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
//! N8 Empty after cleaning -> Empty. The caller then uses `fallback_name`.
//! N9 Collisions on disk: `disambiguate` adds " (2)", " (3)", ... before the extension,
//!    lowest free number; the record keeps the original name (PLAN Storage).
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
    #[error("extension '{0}' is not an allowed photo or video type")]
    ExtensionNotAllowed(String),
    #[error("file content is not a photo or video")]
    NotMedia,
    #[error("extension '{ext}' does not match the file content ({detected:?})")]
    MagicMismatch { ext: String, detected: Family },
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

/// The on-disk file name for a received item: clean_stem(record name without its extension)
/// + "." + the checked ext; on Err(Empty | Reserved | Colon | ...) uses
///   `fallback_name(captured_at, family)` instead (the file is still accepted; only N7
///   failures refuse the file). Returns (name, family).
pub fn disk_file_name(record_name: &str, ext: &str, head: &[u8], captured_at_ms: i64) -> Result<(String, Family), NameError> {
    // N7 first: a type failure refuses the file whatever the name looks like.
    let (ext, family) = check_media(ext.trim_start_matches('.'), head)?;
    let room = MAX_NAME_UTF16.saturating_sub(1 + ext.len());
    let stem = match clean_stem(strip_ext(record_name, &ext)) {
        Ok(s) => {
            let capped = cap_utf16(&s, room);
            if capped.is_empty() {
                fallback_stem(captured_at_ms)
            } else {
                capped
            }
        }
        Err(_) => fallback_stem(captured_at_ms),
    };
    Ok((format!("{stem}.{ext}"), family))
}

/// Folder-name rules (see module doc).
pub fn clean_folder_name(raw: &str) -> Result<String, NameError> {
    let s = clean_common(raw)?;
    if s.starts_with('.') {
        return Err(NameError::LeadingDot);
    }
    check_reserved(&s)?;
    let s = cap_utf16(&s, MAX_NAME_UTF16);
    if s.is_empty() {
        return Err(NameError::Empty);
    }
    Ok(s)
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
    // A trailing ".xyz" of 1-5 letters/digits counts as an extension.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && (1..=5).contains(&(name.len() - i - 1)) && name[i + 1..].chars().all(|c| c.is_ascii_alphanumeric()) => {
            (&name[..i], &name[i..])
        }
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
        assert!(matches!(disk_file_name(name, "exe", &jpeg(), 0), Err(NameError::ExtensionNotAllowed(_))));
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
        assert_eq!(fam, Family::Jpeg);
        let (name, _) = disk_file_name("NUL .jpg", "jpg", &jpeg(), 1_794_000_000_000).unwrap();
        assert!(name.starts_with("IMG_"), "{name}");
        let (name, _) = disk_file_name("a:b.jpg", "jpg", &jpeg(), 1_794_000_000_000).unwrap();
        assert!(name.starts_with("IMG_"), "colon falls back: {name}");
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
}
