//! Thumbnails for files dropped into the library, made with the Windows Shell thumbnail API
//! (the same pictures Explorer shows; no codec of our own). Output is a JPEG, longest side
//! at most 512 px, the format Core's thumbnail contract expects. Best effort: a file without
//! a Shell thumbnail simply gets none, and the gallery shows an icon.

use std::{io, path::Path};

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::SIZE,
        Graphics::Gdi::{
            DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
        },
        System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED},
        UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY},
    },
};

pub const MAX_SIDE: i32 = 512;

fn werr(what: &str, e: windows::core::Error) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

/// Write a JPEG thumbnail of `src` to `out`.
pub fn make_thumbnail(src: &Path, out: &Path) -> io::Result<()> {
    let (w, h, rgb) = shell_rgb(src)?;
    let enc = jpeg_encoder::Encoder::new_file(out, 85).map_err(|e| io::Error::other(e.to_string()))?;
    enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| io::Error::other(e.to_string()))
}

/// The Shell's thumbnail of `src` as tightly packed RGB (width, height, bytes).
fn shell_rgb(src: &Path) -> io::Result<(usize, usize, Vec<u8>)> {
    let wide: Vec<u16> = src.as_os_str().to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
    // The Shell needs COM on this thread. S_FALSE (already initialised) is fine and still needs
    // the matching uninitialise; a different apartment model (RPC_E_CHANGED_MODE) is also fine
    // but must NOT be uninitialised.
    let com_ok = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result = (|| {
        let factory: IShellItemImageFactory = unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }.map_err(|e| werr("shell item", e))?;
        let bmp = unsafe { factory.GetImage(SIZE { cx: MAX_SIDE, cy: MAX_SIDE }, SIIGBF_THUMBNAILONLY) }.map_err(|e| werr("no shell thumbnail", e))?;
        let pixels = read_bitmap(bmp);
        unsafe {
            let _ = DeleteObject(HGDIOBJ(bmp.0));
        }
        pixels
    })();
    if com_ok {
        unsafe { CoUninitialize() };
    }
    result
}

/// Read a shell bitmap (32-bit BGRA) as tightly packed RGB. GetDIBits with BI_RGB leaves the
/// fourth byte of each pixel undefined, and shell thumbnails of photos often come back with it
/// 0; `bgra_premultiplied_to_rgb` handles both cases.
fn read_bitmap(bmp: windows::Win32::Graphics::Gdi::HBITMAP) -> io::Result<(usize, usize, Vec<u8>)> {
    let mut info = BITMAP::default();
    let got = unsafe { GetObjectW(HGDIOBJ(bmp.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut info as *mut BITMAP as *mut _)) };
    if got == 0 || info.bmWidth <= 0 || info.bmHeight <= 0 {
        return Err(io::Error::other("shell bitmap has no size"));
    }
    let (w, h) = (info.bmWidth as usize, info.bmHeight as usize);
    let mut header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // negative = top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bgra = vec![0u8; w * h * 4];
    let dc = unsafe { GetDC(None) };
    let lines = unsafe { GetDIBits(dc, bmp, 0, h as u32, Some(bgra.as_mut_ptr() as *mut _), &mut header, DIB_RGB_COLORS) };
    unsafe { ReleaseDC(None, dc) };
    if lines == 0 {
        return Err(io::Error::other("could not read the shell bitmap"));
    }
    Ok((w, h, bgra_premultiplied_to_rgb(&bgra)))
}

/// BGRA -> RGB. When every pixel's fourth byte is 0 the bitmap has no alpha (BI_RGB leaves the
/// byte undefined, and an all-zero byte would turn every pixel white): it is opaque. Otherwise
/// the alpha is premultiplied and the pixel is composited on white (c + (255 - a)).
pub fn bgra_premultiplied_to_rgb(bgra: &[u8]) -> Vec<u8> {
    let (pixels, _) = bgra.as_chunks::<4>();
    if pixels.iter().all(|px| px[3] == 0) {
        return pixels.iter().flat_map(|px| [px[2], px[1], px[0]]).collect();
    }
    let mut rgb = Vec::with_capacity(bgra.len() / 4 * 3);
    for px in pixels {
        let bg = 255 - px[3] as u16;
        rgb.push((px[2] as u16 + bg).min(255) as u8);
        rgb.push((px[1] as u16 + bg).min(255) as u8);
        rgb.push((px[0] as u16 + bg).min(255) as u8);
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_pixels_become_white_and_opaque_pixels_keep_colour() {
        // B, G, R, A
        let rgb = bgra_premultiplied_to_rgb(&[0, 0, 0, 0, 10, 20, 30, 255]);
        assert_eq!(rgb, vec![255, 255, 255, 30, 20, 10]);
    }

    #[test]
    fn a_bitmap_without_alpha_is_opaque_not_white() {
        // B, G, R, 0 on every pixel: the fourth byte means nothing.
        assert_eq!(bgra_premultiplied_to_rgb(&[10, 20, 30, 0]), vec![30, 20, 10]);
        assert_eq!(bgra_premultiplied_to_rgb(&[10, 20, 30, 0, 1, 2, 3, 0]), vec![30, 20, 10, 3, 2, 1]);
    }

    #[test]
    fn the_shell_thumbnail_of_a_solid_photo_keeps_its_colour() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("red.jpg");
        let px: Vec<u8> = (0..64 * 64).flat_map(|_| [200u8, 40, 40]).collect();
        jpeg_encoder::Encoder::new_file(&src, 95).unwrap().encode(&px, 64, 64, jpeg_encoder::ColorType::Rgb).unwrap();
        let (w, h, rgb) = shell_rgb(&src).expect("shell thumbnail of a real JPEG");
        assert_eq!(rgb.len(), w * h * 3);
        let mean = |c: usize| rgb.as_chunks::<3>().0.iter().map(|p| p[c] as u64).sum::<u64>() / (w * h) as u64;
        let (r, g, b) = (mean(0) as i64, mean(1) as i64, mean(2) as i64);
        assert!((r - 200).abs() < 25 && (g - 40).abs() < 25 && (b - 40).abs() < 25, "mean colour {r},{g},{b} is not the source (200,40,40): a white thumbnail is the bug");
    }

    #[test]
    fn a_real_jpeg_gets_a_jpeg_thumbnail_and_a_non_image_gets_none() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("photo.jpg");
        let px: Vec<u8> = (0..64 * 64).flat_map(|i| [(i % 64 * 4) as u8, (i / 64 * 4) as u8, 120]).collect();
        jpeg_encoder::Encoder::new_file(&src, 90).unwrap().encode(&px, 64, 64, jpeg_encoder::ColorType::Rgb).unwrap();
        let out = d.path().join("thumb.jpg");
        make_thumbnail(&src, &out).expect("shell thumbnail of a real JPEG");
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(&[0xFF, 0xD8, 0xFF]) && bytes.len() > 200, "a JPEG file");

        let txt = d.path().join("notes.txt");
        std::fs::write(&txt, b"hello").unwrap();
        assert!(make_thumbnail(&txt, &d.path().join("t2.jpg")).is_err(), "no thumbnail for text");
    }
}
