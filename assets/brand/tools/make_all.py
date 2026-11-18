"""Build every Tugboat brand SVG from geom.json, then render them through Electron.
Usage: py -3.11 make_all.py [--no-render]"""
import os
import sys
import numpy as np
from PIL import Image, ImageDraw

from lib import *
from render import render_svg

B = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
R = os.environ.get("TUG_RENDERS", B + r"\renders")
os.makedirs(R, exist_ok=True)
RENDER = "--no-render" not in sys.argv

WHITE = "#FFFFFF"
SEA = "#0F3D5E"  # proposed icon / adaptive background
g = load()
x0, y0, x1, y1 = bbox(g)
bw, bh = x1 - x0, y1 - y0
bcx, bcy = (x0 + x1) / 2, (y0 + y1) / 2
files = []


def write(path, text):
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        f.write(text)
    files.append(path)


def draw(svg, out, w, h, bg=None, flat=True):
    if RENDER:
        render_svg(svg, out, w, h, bg=bg, flat=flat and bg is None)


# ------------------------------------------------------------ 1. full logo (aligned to the source pixel grid)
OX, OY, VBW, VBH = 175, 495, 904, 362
write(B + r"\tugboat-logo.svg", svg_doc(VBW, VBH, paths_svg(g, Xf(1.0, OX, OY, 0, 0))))

# ------------------------------------------------------------ 2. square mark: 1024 viewBox, boat 800 wide
MARK_W = 800.0
s_mark = MARK_W / bw
xf_mark = Xf(s_mark, bcx, bcy, 512, 512)
mark_body = paths_svg(g, xf_mark)
write(B + r"\tugboat-mark-square.svg", svg_doc(1024, 1024, mark_body, width=1024, height=1024, title="Tugboat"))
# solid-background tile (Play Store 512, full bleed) and a rounded Windows tile
write(B + r"\tugboat-play-512.svg", svg_doc(1024, 1024, f'<rect width="1024" height="1024" fill="{SEA}"/>\n<g fill="{WHITE}">\n{mark_body}</g>\n', fill=WHITE, width=512, height=512))
# Windows app-icon tile (also used for the tray): the full artwork on
# the sea tile, boat 900 wide (88 %) so it still reads at the 16-32 px taskbar and tray sizes. It stays clear of the
# rounded corners (boat 360 high, centred: y 332-692; the corners only bend above y 224 and below y 800).
TILE_W = 900.0
TILE_RECT = f'<rect width="1024" height="1024" rx="224" fill="{SEA}"/>'
tile_body = paths_svg(g, Xf(TILE_W / bw, bcx, bcy, 512, 512))
BOAT = f'<g fill="{WHITE}">\n{tile_body}</g>\n'
write(B + r"\tugboat-tile-1024.svg", svg_doc(1024, 1024, f"{TILE_RECT}\n{BOAT}", fill=WHITE, width=1024, height=1024))

# ------------------------------------------------------------ 3. Android adaptive icon, 108 x 108
# centre on the boat's bounding box (visual centre); scale so the farthest silhouette point sits on the safe-zone radius
pts = []
for k in ("cap", "cabin", "hull", "wave"):
    for c in g[k]:
        c = np.array(c)
        t = np.linspace(0, 1, 80)[:, None]
        pts.append((1 - t) ** 3 * c[0] + 3 * (1 - t) ** 2 * t * c[1] + 3 * (1 - t) * t * t * c[2] + t ** 3 * c[3])
P = np.vstack(pts)
ecx, ecy = bcx, bcy
er = float(np.hypot(P[:, 0] - ecx, P[:, 1] - ecy).max())
SAFE_R = 32.8  # dp, inside the 33 dp (66 dp) safe radius
s_and = SAFE_R / er
xf_and = Xf(s_and, ecx, ecy, 54, 54)
and_body = paths_svg(g, xf_and)
and_w, and_h = bw * s_and, bh * s_and
write(B + r"\android-foreground.svg", svg_doc(108, 108, and_body, fill=WHITE, width=108, height=108))
write(B + r"\android-monochrome.svg", svg_doc(108, 108, and_body, fill="#000000", width=108, height=108))
write(B + r"\android-background.svg", svg_doc(108, 108, f'<rect width="108" height="108" fill="{SEA}"/>\n', fill=SEA, width=108, height=108))

# ------------------------------------------------------------ 4. tray icons: the app tile in three states
# Told apart by SHAPE, colour only helps: waiting = a dot top right (buoy orange, cut out with a sea ring),
# offline = a slash top-left to bottom-right (white, with a halo cut through the boat) on a grey tile.
# Drawn from the vector at every tray size, so the tray is the same artwork as the app icon (synced == app tile).
BUOY = "#FF8E45"
GREY = "#5B6670"  # white on it 5.9:1; it on the light taskbar #F3F3F3 5.4:1
TRAY_SIZES = (16, 20, 24, 32, 40, 48)
SLASH = 'x1="170" y1="170" x2="854" y2="854" stroke-linecap="round"'
tray_art = {
    "synced": f"{TILE_RECT}\n{BOAT}",
    "waiting": f'{TILE_RECT}\n{BOAT}<circle cx="812" cy="212" r="200" fill="{SEA}"/>\n<circle cx="812" cy="212" r="150" fill="{BUOY}"/>\n',
    "offline": f'{TILE_RECT.replace(SEA, GREY)}\n{BOAT}<line {SLASH} stroke="{GREY}" stroke-width="230"/>\n<line {SLASH} stroke="{WHITE}" stroke-width="96"/>\n',
}
tray_svgs = {}
for state, body in tray_art.items():
    path = B + rf"\tray-{state}.svg"
    write(path, svg_doc(1024, 1024, body, fill=WHITE, width=1024, height=1024))
    tray_svgs[state] = path


info = dict(er=er, ecx=ecx, ecy=ecy, s_and=s_and, and_w=and_w, and_h=and_h, s_mark=s_mark, bw=bw, bh=bh,
            mark_h=bh * s_mark)
for k, v in info.items():
    print(k, round(float(v), 4))

if not RENDER:
    sys.exit(0)

# ------------------------------------------------------------ renders
def lw(w):
    return int(round(w * VBH / VBW))


for w in (1024, 512, 192, 48, 32, 16):
    draw(B + r"\tugboat-logo.svg", f"{R}\\tugboat-logo-{w}.png", w, lw(w))
for sz in (1024, 512, 192, 48, 32, 16):
    draw(B + r"\tugboat-mark-square.svg", f"{R}\\tugboat-mark-square-{sz}.png", sz, sz)
for sz in (512, 192, 48):
    draw(B + r"\tugboat-play-512.svg", f"{R}\\tugboat-play-{sz}.png", sz, sz, bg=SEA)
for sz in (1024, 256, 64, 48, 40, 32, 24, 20, 16):  # every app .ico size, from the vector
    draw(B + r"\tugboat-tile-1024.svg", f"{R}\\tugboat-tile-{sz}.png", sz, sz, flat=False)
# deliverable rasters in the brand root
draw(B + r"\tugboat-mark-square.svg", B + r"\tugboat-1024.png", 1024, 1024)
draw(B + r"\tugboat-play-512.svg", B + r"\tugboat-play-512.png", 512, 512, bg=SEA)
for sz in (432, 192, 108, 48):
    draw(B + r"\android-foreground.svg", f"{R}\\android-foreground-{sz}.png", sz, sz)
for sz in (192, 48):
    draw(B + r"\android-monochrome.svg", f"{R}\\android-monochrome-{sz}.png", sz, sz)
for state, path in tray_svgs.items():
    for N in TRAY_SIZES:
        draw(path, f"{R}\\tray-{N}-{state}.png", N, N, flat=False)

# ------------------------------------------------------------ preview sheets (Pillow only)
def over(bg, fg):
    im = Image.new("RGBA", fg.size, bg)
    im.alpha_composite(fg)
    return im


# android masks preview: foreground over background with circle and rounded-square masks + safe-zone guides
fg = Image.open(f"{R}\\android-foreground-432.png").convert("RGBA")
bgc = Image.new("RGBA", (432, 432), SEA)
bgc.alpha_composite(fg)
full = bgc.copy()
dg = ImageDraw.Draw(full)
cx = 216
for diam, col in ((72, (255, 255, 255, 255)), (66, (231, 155, 37, 255))):
    r = diam * 4 / 2
    dg.ellipse([cx - r, cx - r, cx + r, cx + r], outline=col, width=2)


def masked(img, shape):
    m = Image.new("L", img.size, 0)
    dm = ImageDraw.Draw(m)
    r = 72 * 4 / 2
    if shape == "circle":
        dm.ellipse([216 - r, 216 - r, 216 + r, 216 + r], fill=255)
    else:
        dm.rounded_rectangle([216 - r, 216 - r, 216 + r, 216 + r], radius=int(r * 0.45), fill=255)
    out = Image.new("RGBA", img.size, (235, 235, 235, 255))
    out.paste(img, (0, 0), m)
    return out


sheet = Image.new("RGBA", (432 * 3 + 40, 432 + 40), (235, 235, 235, 255))
sheet.paste(full, (10, 20))
sheet.paste(masked(bgc, "circle"), (432 + 20, 20))
sheet.paste(masked(bgc, "squircle"), (432 * 2 + 30, 20))
d2 = ImageDraw.Draw(sheet)
d2.text((12, 4), "108 dp layer: white ring = 72 dp visible area, amber ring = 66 dp safe zone", fill=(0, 0, 0, 255))
d2.text((432 + 22, 4), "circle mask", fill=(0, 0, 0, 255))
d2.text((432 * 2 + 32, 4), "rounded-square mask", fill=(0, 0, 0, 255))
sheet.convert("RGB").save(f"{R}\\android-preview-masks.png")

# monochrome preview
mono = Image.open(f"{R}\\android-monochrome-192.png").convert("RGBA")
ms = Image.new("RGBA", (192 * 2 + 30, 192 + 20), (235, 235, 235, 255))
ms.alpha_composite(mono, (10, 10))
tint = Image.new("RGBA", mono.size, (0, 0, 0, 0))
arr = np.asarray(mono).copy()
arr[..., :3] = (60, 120, 160)
ms.alpha_composite(Image.fromarray(arr), (192 + 20, 10))
ms.convert("RGB").save(f"{R}\\android-monochrome-preview.png")

# tray sheet: every state at every size, 4x (nearest) with a 1:1 strip, on the light and the dark taskbar
SC = 4
SIZES = TRAY_SIZES
colw = sum(n * SC + 16 for n in SIZES) + 150
rowh = 48 * SC + 2 + 48 + 8
sheet = Image.new("RGB", (colw, 2 * (3 * rowh + 24)), (255, 255, 255))
dd = ImageDraw.Draw(sheet)
yy = 0
for tag, bgcol in (("light taskbar", (243, 243, 243)), ("dark taskbar", (32, 32, 32))):
    dd.rectangle([0, yy, sheet.width, yy + 3 * rowh + 24], fill=bgcol)
    dd.text((8, yy + 4), tag + "   sizes 16 20 24 32 40 48 at 4x, then 1:1 below each icon", fill=(120, 120, 120))
    yy += 24
    for state in ("synced", "waiting", "offline"):
        xx = 8
        for n in SIZES:
            im = Image.open(f"{R}\\tray-{n}-{state}.png").convert("RGBA")
            big = im.resize((n * SC, n * SC), Image.NEAREST)
            sheet.paste(over(bgcol + (255,), big).convert("RGB"), (xx, yy))
            sheet.paste(over(bgcol + (255,), im).convert("RGB"), (xx, yy + 48 * SC + 2))
            xx += n * SC + 16
        dd.text((xx + 4, yy + 8), state, fill=(150, 150, 150))
        yy += rowh
sheet.crop((0, 0, sheet.width, yy)).save(f"{R}\\tray-states-sheet.png")

# size ladder: mark on light and dark at 16/32/48/192
ladder = Image.new("RGB", (192 + 48 + 32 + 16 + 100, 192 * 2 + 30), (255, 255, 255))
for j, bgcol in enumerate(((243, 243, 243), (32, 32, 32))):
    y = j * (192 + 10) + 5
    ImageDraw.Draw(ladder).rectangle([0, y - 5, ladder.width, y + 192 + 5], fill=bgcol)
    x = 10
    for sz in (192, 48, 32, 16):
        im = Image.open(f"{R}\\tugboat-mark-square-{sz}.png").convert("RGBA")
        ladder.paste(over(bgcol + (255,), im).convert("RGB"), (x, y))
        x += sz + 20
ladder.save(f"{R}\\mark-size-ladder.png")
import make_ico
make_ico.build()
print("rendered")
