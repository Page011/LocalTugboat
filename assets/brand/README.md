> The logo kit as shipped. The rebuild tools live in `tools/` and write into this folder (renders go to
> `TUG_RENDERS` or `renders/`, not in git). Regenerate the Windows app icons with `bun scripts/icons.ts` in `apps/desktop`. The leftmost
> porthole is cut open by the hull's bottom edge ON PURPOSE (a signature detail of the design): never move, close or bridge it.

# Tugboat brand kit

Source: `Tugboat.png` (1254 x 1254), the original artwork, an own composite; the tools find it through the `TUG_SOURCE` environment variable. Everything here is vector, traced from that PNG and measured against it. The licences of the icons the composite draws on were not checked.

## Colours

| Use | Value | Note |
|---|---|---|
| Logo ink | `#1A2733` | Measured: RGB 26,39,51 is the most common dark pixel (37,625 px) in the PNG. The file says `#1A2733`, not the `#1C2A3A` first assumed. |
| Icon / adaptive background (proposed) | `#0F3D5E` | Deep sea blue. White on it = 11.35:1 contrast. Used by `tugboat-play-512`, `tugboat-tile-1024`, `android-background`. |
| Boat on dark | `#FFFFFF` | Play icon, tile, tray, Android foreground. |
| Tray waiting dot | `#FF8E45` | Buoy orange (the app's rail accent), cut out of the tile with a sea ring. |
| Tray offline tile | `#5B6670` | Grey. White boat and slash on it 5.9:1; the tile on a light taskbar 5.4:1. |

Navy `#1A2733` on white = 15.2:1.

## Files

| File | Use |
|---|---|
| `tugboat-logo.svg` | Full logo. viewBox `0 0 904 362` = source pixels, origin at source (175,495). Single `fill` on the root, four `<path fill-rule="evenodd">` (cap, cabin + window, hull + 4 portholes, wave). No raster, filter, CSS or `<use>`. 108 cubic segments (fit tolerance 0.3 px) plus 4 circle arcs. |
| `tugboat-mark-square.svg`, `tugboat-1024.png` | Logo centred in a 1024 square, transparent. Boat 800 wide (78 %), 320 high, 112 px side padding. Windows icon master and general use on light backgrounds. Navy: invisible on dark, use the tile or white variants there. |
| `tugboat-play-512.svg/.png` | Play Store icon: full-bleed `#0F3D5E` square, white boat 400 px wide. Square, no rounding (Play applies its own mask). |
| `tugboat-tile-1024.svg` | Windows app icon master: the artwork in white on a rounded `#0F3D5E` tile (r 224), boat 900 wide (88 %) so it reads at taskbar and tray sizes; it stays clear of the rounded corners. The full artwork is used at every size, with no simplified small boat. |
| `android-foreground.svg` | 108 x 108 dp, white boat. 63.6 x 25.4 dp, centred on the boat's bounding box at (54,54). Farthest silhouette point is 32.8 dp from centre, inside the 33 dp radius of the 66 dp safe zone, so no mask can clip it. Fills 88 % of the 72 dp visible window (the PNG fills 72 % of its frame width). |
| `android-monochrome.svg` | Same geometry, single colour (black). Android 13+ themed icons use only its alpha. |
| `android-background.svg` | Solid `#0F3D5E`. In `ic_launcher_background` use the colour `#0F3D5E`. |
| `tray-synced.svg`, `tray-waiting.svg`, `tray-offline.svg` | Tray icon in three states, built on the same tile as the app icon. Synced = the app tile exactly. Waiting = the tile plus an orange dot top right. Offline = grey tile with a white slash top-left to bottom-right (a grey halo cuts it out of the boat). |
| `ico/tray-{synced,waiting,offline}.ico` | One `.ico` per state, 16, 20, 24, 32, 40, 48 (PNG entries) drawn from the vector at each size. One set: the tile reads on light and dark taskbars. The app picks the entry for the display scale (table under Tray). |
| `ico/tugboat-app.ico` | App / exe / taskbar icon: the tile drawn from the vector at 256, 64, 48, 40, 32, 24, 20, 16 (largest first). The tray `synced` entries are byte-identical to the same sizes here. |
| `renders/` | PNG renders of every SVG, comparison images, preview sheets (below). |
| `tools/` | Scripts that produced all of it (see Rebuild). |

## Design notes

1. **Leftmost porthole: KEPT EXACTLY AS DRAWN.** In the PNG its circle (centre 497.5, 757.9, r 24.9 source px) runs through the hull's bottom edge, opening the hull into the gap above the waterline. This is a deliberate, signature part of the design. The hull outline keeps its traced notch; nothing is bridged, moved or "fixed". `tools/fit.py` `KEEP_ORIGINAL_PORTHOLE = True`; never change it.
2. **Small sizes.** None: the app icon and tray are the full artwork at every size. An earlier simplified pixel boat was dropped.
3. **Phone icon.** Boat larger in the frame, see `android-foreground.svg` above.

## Measured match: `tugboat-logo.svg` vs the PNG

Method: SVG rendered through Electron at 1:1 (904 x 362, white), placed on the PNG's 1254 grid at (175,495); both turned into a mask at 50 % between ink and paper (grey 146); compared pixel by pixel.

| | Whole image | Porthole zone excluded (rect x 455-540, y 700-795) |
|---|---|---|
| Dark pixels in the PNG | 135,736 | 130,436 |
| Differing pixels | **633 (0.466 %)** | 609 (0.467 %) |
| Overlap (IoU) | **99.535 %** | 99.534 % |
| Pixels further than 1 px from the other outline | 1 | 0 |
| Mean absolute grey difference | 0.68 / 255 | 0.67 / 255 |

(Measured with the leftmost porthole kept exactly as drawn: the whole image, porthole included, matches the PNG to within the 1 px edge fringe.) Nothing is more than 1 px off apart from a single pixel; the 569 px are the 1 px anti-aliased fringe of a soft-edged PNG. Fit tolerance was iterated: 0.45 px = 764 differing px (89 segments), 0.3 px = 569 (108, shipped), 0.2 px = 487 (125); the remaining ~0.37 % is the PNG's own edge softness, not worth more segments. Largest deviation of any fitted curve from the traced outline: 0.38 px (cap 0.31, hull 0.38, wave 0.35, window 0.36), except the cabin's two sharp corners 0.78 px.

Images: `renders/compare-side-by-side.png` (PNG left, SVG right, same scale; both panels in true colour, the SVG panel is the real RGB render of the `#1A2733` fill on white, greyscale is used only inside the mask maths), `renders/compare-diff-overlay.png` (grey both, red PNG only, blue SVG only, green box = the porthole zone excluded in the table).

## Tray

Windows tray icon size follows display scaling, so the app ships every size and picks one (a resampled image is blurry):

| Display scaling | Tray icon px |
|---|---|
| 100 % | 16 |
| 125 % | 20 |
| 150 % | 24 |
| 200 % | 32 |
| 250 % | 40 |
| 300 % | 48 |

Odd scalings (175 %, 225 %) take the nearest, the larger on a tie. At 16 px the portholes and window are below a pixel and the boat reads as its outline; that was chosen over a simplified boat.

- Synced: the plain tile (same image as the app icon).
- Waiting: orange dot top right (r 150 of 1024, with a sea ring to r 200).
- Offline: grey tile, white slash top-left to bottom-right (96 of 1024 wide, grey halo 230).
- Meaning never depends on colour: dot = waiting, slash = offline. `tools/check_render.py` checks every size: synced equals the app icon, the dot changes the top right, offline differs.

## Renders (`renders/`)

- `tugboat-logo-{1024,512,192,48,32,16}.png` (by width, transparent)
- `tugboat-mark-square-{1024,512,192,48,32,16}.png` (transparent), `mark-size-ladder.png`
- `tugboat-play-{512,192,48}.png`, `tugboat-tile-{1024,256,64,48,40,32,24,20,16}.png`
- `android-foreground-{432,192,108,48}.png`, `android-monochrome-{192,48}.png`, `android-preview-masks.png` (circle and rounded-square masks with the 72 dp and 66 dp guides), `android-monochrome-preview.png`
- `tray-{16,20,24,32,40,48}-{synced,waiting,offline}.png`, `tray-states-sheet.png`
- `ico-sizes-sheet.png` (every entry of `ico/tugboat-app.ico` at 1:1 and 4x, light and dark)
- `compare-side-by-side.png`, `compare-diff-overlay.png`

## Known limits

- The four portholes are 3.5 dp across in the Android icon; they soften below about 48 px.
- Navy artwork on a dark surface is unreadable (`mark-size-ladder.png`, lower half). Use the white or tile variants there.
- The ico PNG entries need Windows Vista or later (all supported systems). Rust/Tauri: embed `ico/tugboat-app.ico` as the exe icon and the matching `tray-*.ico` for the tray.

## Rebuild

Needs Python 3.11 with numpy, scipy, Pillow, and Electron (`TUG_ELECTRON`, else `electron` on PATH; see `tools/render.py`), and `TUG_SOURCE` set to the path of `Tugboat.png`. From `tools/`: `py -3.11 trace.py` (PNG to outline loops), `py -3.11 fit.py 0.3` (loops to `geom.json`, prints porthole and fit numbers), `py -3.11 make_all.py` (all SVGs, renders, `.ico` files via `make_ico.py`), `py -3.11 check_render.py` (`.ico` sizes and tray state checks), `py -3.11 make_logo.py` then `py -3.11 compare.py` (match numbers and comparison images). `trace.py` writes `loops.json` next to itself (regenerable). `make_logo.py` and `compare.py` write two scratch PNGs next to themselves (`logo_1to1.png`, `logo_on_canvas.png`). `shoot.cjs` is the Electron renderer (takes the output path, page, size and an optional transparency flag).
