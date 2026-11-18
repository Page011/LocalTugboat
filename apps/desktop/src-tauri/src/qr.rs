//! The pairing QR, drawn in Rust as an SVG string (the webview only shows it). Always black on
//! white with a quiet zone, whatever the app theme: scanners need that contrast.

use qrcode::{render::svg, EcLevel, QrCode};

/// Longest payload the core accepts is 512 bytes; at error level L that fits comfortably.
pub fn svg(payload: &str) -> Result<String, String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::L).map_err(|e| format!("QR CODE FAILED - {e}"))?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(260, 260)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

/// The `e=` expiry (unix ms, PC clock) of a pairing payload: the UI counts down to it.
pub fn expiry_of(payload: &str) -> Option<i64> {
    payload.split_once('?')?.1.split('&').find_map(|kv| kv.strip_prefix("e=")).and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> String {
        format!("localtugboat://pair?v=1&k={}&a=192.168.1.20:47300&s={}&e=1790000120000&n=NIGEL-PC", "ab".repeat(32), "cd".repeat(16))
    }

    #[test]
    fn makes_a_black_on_white_svg_with_a_quiet_zone() {
        let s = svg(&payload()).unwrap();
        assert!(s.starts_with("<?xml") || s.contains("<svg"), "{}", &s[..40.min(s.len())]);
        assert!(s.contains("#000000") && s.contains("#ffffff"));
        // The same text always gives the same picture.
        assert_eq!(s, svg(&payload()).unwrap());
        assert_ne!(s, svg(&payload().replace("NIGEL", "OTHER")).unwrap());
    }

    #[test]
    fn a_maximum_size_payload_still_fits_and_an_oversize_one_is_an_error() {
        assert!(svg(&"a".repeat(512)).is_ok());
        assert!(svg(&"a".repeat(4000)).is_err(), "beyond QR capacity at level L");
    }

    #[test]
    fn reads_the_expiry() {
        assert_eq!(expiry_of(&payload()), Some(1_790_000_120_000));
        assert_eq!(expiry_of("localtugboat://pair?v=1&k=00"), None);
        assert_eq!(expiry_of("garbage"), None);
        assert_eq!(expiry_of("localtugboat://pair?e=abc"), None);
    }
}
