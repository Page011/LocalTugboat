//! The tray icon: the Tugboat tile in three states (synced, waiting, offline), told apart by SHAPE
//! (plain tile, a dot top right, a slash across a grey tile) so colour is never the only signal.
//!
//! The icons are the owner's artwork on the sea tile, the same as the app icon (owner 2026-11-18: "use the artwork
//! on tile"): assets/brand/ico/tray-<state>.ico, each holding six PNG entries drawn from the vector at 16, 20, 24, 32,
//! 40 and 48 px. Windows draws the tray at 16 px x the display scale, so the entry is picked to match (a resampled
//! image is blurry). The tile reads on light and dark taskbars alike, so there is one set.

use serde::{Deserialize, Serialize};
use tugboat_core::events::{SyncState, SyncStatus};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TrayState {
    /// Connected and everything confirmed.
    Synced,
    /// Connecting, catching up, downloading, or items waiting for the other device.
    Waiting,
    /// No phone paired, phone not reachable, paused or refused.
    Offline,
}

/// The tray state for a sync status.
pub fn tray_state(s: &SyncStatus) -> TrayState {
    match s.state {
        SyncState::Synced if s.waiting == 0 && s.downloading == 0 => TrayState::Synced,
        SyncState::Synced | SyncState::Syncing | SyncState::Connecting => TrayState::Waiting,
        SyncState::Offline | SyncState::Unpaired | SyncState::Paused | SyncState::Refused => TrayState::Offline,
    }
}

/// Tooltip: one short line.
pub fn tooltip(state: TrayState, s: &SyncStatus) -> String {
    let who = s.peer_name.as_deref().unwrap_or("no phone");
    match state {
        TrayState::Synced => format!("Tugboat - synced with {who}"),
        TrayState::Waiting if s.waiting > 0 => format!("Tugboat - {} waiting for {who}", s.waiting),
        TrayState::Waiting => format!("Tugboat - syncing with {who}"),
        TrayState::Offline => match (&s.state, &s.message) {
            (SyncState::Unpaired, _) => "Tugboat - no phone paired".to_string(),
            (_, Some(m)) => format!("Tugboat - {m}"),
            _ => format!("Tugboat - {who} not connected"),
        },
    }
}

/// Edges the kit draws, smallest first.
pub const SIZES: [u32; 6] = [16, 20, 24, 32, 40, 48];

/// The glyph edge for a monitor scale factor: 16 px x scale (what Windows asks for, GetSystemMetricsForDpi(SM_CXSMICON)),
/// then the nearest drawn size, the larger one on a tie (175% -> 32, 225% -> 40), clamped to 16..48.
pub fn size_for_scale(scale: f64) -> u32 {
    let want = 16.0 * if scale.is_finite() { scale } else { 1.0 };
    let mut best = SIZES[0];
    let mut best_d = f64::MAX;
    for s in SIZES {
        let d = (f64::from(s) - want).abs();
        if d <= best_d {
            best = s;
            best_d = d;
        }
    }
    best
}

/// The kit's `.ico` for a state: six PNG entries.
fn ico(state: TrayState) -> &'static [u8] {
    match state {
        TrayState::Synced => include_bytes!("../../../../assets/brand/ico/tray-synced.ico"),
        TrayState::Waiting => include_bytes!("../../../../assets/brand/ico/tray-waiting.ico"),
        TrayState::Offline => include_bytes!("../../../../assets/brand/ico/tray-offline.ico"),
    }
}

/// The PNG bytes of the entry that is exactly `size` x `size` in an `.ico`. The directory is bounds-checked: a
/// damaged file gives `None`, never a panic.
fn ico_entry(ico: &[u8], size: u32) -> Option<&[u8]> {
    let count = usize::from(u16::from_le_bytes(ico.get(4..6)?.try_into().ok()?));
    for i in 0..count {
        let e = ico.get(6 + i * 16..22 + i * 16)?;
        let edge = |b: u8| if b == 0 { 256 } else { u32::from(b) };
        if edge(e[0]) == size && edge(e[1]) == size {
            let len = u32::from_le_bytes(e[8..12].try_into().ok()?) as usize;
            let off = u32::from_le_bytes(e[12..16].try_into().ok()?) as usize;
            return ico.get(off..off.checked_add(len)?);
        }
    }
    None
}

/// RGBA pixels of the glyph (size x size x 4 bytes). Never panics: a missing or undecodable entry is an error text.
pub fn glyph_rgba(state: TrayState, size: u32) -> Result<Vec<u8>, String> {
    let png_bytes = ico_entry(ico(state), size).ok_or_else(|| format!("no {size} px entry in the {state:?} icon"))?;
    let mut reader = png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info().map_err(|e| format!("tray glyph: {e}"))?;
    let mut buf = vec![0u8; reader.output_buffer_size().ok_or("tray glyph: size overflow")?];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("tray glyph: {e}"))?;
    if (info.width, info.height) != (size, size) || info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!("tray glyph: expected {size} px RGBA 8 bit, got {}x{} {:?} {:?}", info.width, info.height, info.color_type, info.bit_depth));
    }
    buf.truncate((size * size * 4) as usize);
    Ok(buf)
}

/// Does this window message mean the tray icon must be chosen again? A display or DPI change moves the right size.
pub fn needs_new_icon(msg: u32) -> bool {
    const WM_DISPLAYCHANGE: u32 = 0x007E;
    const WM_DPICHANGED: u32 = 0x02E0;
    matches!(msg, WM_DISPLAYCHANGE | WM_DPICHANGED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(state: SyncState, waiting: u32, downloading: u32) -> SyncStatus {
        SyncStatus { state, peer_name: Some("Pixel".into()), last_synced_at: None, waiting, downloading, message: None }
    }

    #[test]
    fn every_sync_state_maps_to_one_of_three() {
        assert_eq!(tray_state(&status(SyncState::Synced, 0, 0)), TrayState::Synced);
        // Connected but something is still pending: not "synced".
        assert_eq!(tray_state(&status(SyncState::Synced, 3, 0)), TrayState::Waiting);
        assert_eq!(tray_state(&status(SyncState::Synced, 0, 2)), TrayState::Waiting);
        assert_eq!(tray_state(&status(SyncState::Syncing, 0, 0)), TrayState::Waiting);
        assert_eq!(tray_state(&status(SyncState::Connecting, 0, 0)), TrayState::Waiting);
        for s in [SyncState::Offline, SyncState::Unpaired, SyncState::Paused, SyncState::Refused] {
            assert_eq!(tray_state(&status(s, 0, 0)), TrayState::Offline, "{s:?}");
        }
    }

    #[test]
    fn tooltips_are_one_line_and_say_why() {
        let t = |s: &SyncStatus| tooltip(tray_state(s), s);
        assert_eq!(t(&status(SyncState::Synced, 0, 0)), "Tugboat - synced with Pixel");
        assert_eq!(t(&status(SyncState::Synced, 4, 0)), "Tugboat - 4 waiting for Pixel");
        assert_eq!(t(&status(SyncState::Unpaired, 0, 0)), "Tugboat - no phone paired");
        let mut paused = status(SyncState::Paused, 0, 0);
        paused.message = Some("LIBRARY FOLDER NOT FOUND - D:\\x".into());
        assert!(t(&paused).contains("LIBRARY FOLDER NOT FOUND"));
        assert!(!t(&status(SyncState::Offline, 0, 0)).contains('\n'));
    }

    const STATES: [TrayState; 3] = [TrayState::Synced, TrayState::Waiting, TrayState::Offline];

    fn px(g: &[u8], size: u32, x: u32, y: u32) -> &[u8] {
        let i = ((y * size + x) * 4) as usize;
        &g[i..i + 4]
    }

    /// Sum of channel differences between two icons over the pixels `keep` accepts.
    fn diff(a: &[u8], b: &[u8], size: u32, keep: impl Fn(u32, u32) -> bool) -> u32 {
        let mut d = 0;
        for y in 0..size {
            for x in 0..size {
                if keep(x, y) {
                    d += px(a, size, x, y).iter().zip(px(b, size, x, y)).map(|(p, q)| u32::from(p.abs_diff(*q))).sum::<u32>();
                }
            }
        }
        d
    }

    #[test]
    fn every_state_and_size_decodes_to_a_rounded_tile() {
        for state in STATES {
            for size in SIZES {
                let g = glyph_rgba(state, size).unwrap_or_else(|e| panic!("{state:?} {size}: {e}"));
                assert_eq!(g.len(), (size * size * 4) as usize);
                assert_eq!(px(&g, size, size / 2, size / 2)[3], 0xFF, "{state:?} {size}: the tile is solid in the middle");
                assert!(px(&g, size, 0, 0)[3] < 0x40, "{state:?} {size}: the corner is rounded off");
            }
        }
        assert!(glyph_rgba(TrayState::Synced, 17).is_err(), "a size the kit does not draw is an error, not a resample");
    }

    #[test]
    fn the_synced_tray_icon_is_the_app_icon() {
        let app: &[u8] = include_bytes!("../../../../assets/brand/ico/tugboat-app.ico");
        for size in SIZES {
            let synced = ico_entry(ico(TrayState::Synced), size).unwrap();
            let same = ico_entry(app, size).unwrap_or_else(|| panic!("app icon lacks {size}"));
            assert_eq!(synced, same, "{size}: tray synced and the app icon are the same artwork");
        }
    }

    #[test]
    fn the_waiting_dot_sits_top_right_and_offline_changes_the_whole_icon() {
        for size in SIZES {
            let [s, w, o] = STATES.map(|st| glyph_rgba(st, size).unwrap());
            let third = size / 3;
            let top_right = |x: u32, y: u32| x >= size - third && y < third;
            assert!(diff(&s, &w, size, top_right) >= 255 * 3, "{size}: the waiting dot is drawn top right");
            assert_eq!(diff(&s, &w, size, |x, y| x < size / 2 && y >= size / 2), 0, "{size}: waiting leaves the rest alone");
            assert!(diff(&s, &o, size, |_, _| true) >= 255 * 3 * size, "{size}: offline is a different icon");
            // The slash runs top-left to bottom-right: the middle of the diagonal is white on the offline icon.
            assert!(px(&o, size, size / 2, size / 2)[..3].iter().all(|&c| c >= 0xF0), "{size}: slash through the middle");
        }
    }

    #[test]
    fn the_icon_size_follows_the_display_scale() {
        for (scale, size) in [(0.75, 16), (1.0, 16), (1.25, 20), (1.5, 24), (1.75, 32), (2.0, 32), (2.25, 40), (2.5, 40), (3.0, 48), (4.0, 48), (f64::NAN, 16)] {
            assert_eq!(size_for_scale(scale), size, "scale {scale}");
        }
    }

    #[test]
    fn only_a_display_or_dpi_change_picks_a_new_icon() {
        assert!(needs_new_icon(0x007E));
        assert!(needs_new_icon(0x02E0));
        assert!(!needs_new_icon(0x001A), "a settings change (light/dark) does not matter: the tile reads on both");
        assert!(!needs_new_icon(0x0001));
    }

    #[test]
    fn a_damaged_ico_is_an_error_not_a_panic() {
        assert!(ico_entry(&[], 16).is_none());
        assert!(ico_entry(&[0, 0, 1, 0, 1, 0, 16, 16], 16).is_none());
        // The directory says the image runs far past the end of the file.
        let mut bad = vec![0, 0, 1, 0, 1, 0, 16, 16, 0, 0, 1, 0, 32, 0];
        bad.extend_from_slice(&1000u32.to_le_bytes());
        bad.extend_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
        assert!(ico_entry(&bad, 16).is_none());
    }
}
