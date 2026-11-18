"""Render an SVG file to PNG through Electron capturePage (img tag, so the real file is what is drawn).
Small sizes: Windows enforces a minimum window size, so the page is rendered on a larger canvas and cropped."""
import os
import subprocess
import tempfile
from pathlib import Path
from PIL import Image

# Any Electron works (it only draws the SVG); set TUG_ELECTRON on a PC without that checkout.
ELECTRON = os.environ.get("TUG_ELECTRON", r"C:\apps\forteConsole\forteops\app\node_modules\electron\dist\electron.exe")
SHOOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "shoot.cjs")
MINC = 320


def render_svg(svg_path, out_png, w, h, bg=None, pad=0, flat=False):
    """bg None = transparent. pad = px margin around the image inside the w x h output."""
    if not os.path.isfile(ELECTRON):
        raise RuntimeError("Electron not found at %s: set TUG_ELECTRON to the path of electron.exe" % ELECTRON)
    svg_path = Path(svg_path).resolve()
    tmpd = Path(tempfile.mkdtemp(prefix="tug_", dir=os.environ.get("TUG_TMP")))
    html = tmpd / "page.html"
    cw, ch = max(w, MINC), max(h, MINC)
    iw, ih = w - 2 * pad, h - 2 * pad
    bgcss = f"background:{bg};" if bg else "background:transparent;"
    html.write_text(
        f'<!doctype html><meta charset="utf-8"><body style="margin:0;background:transparent;width:{cw}px;height:{ch}px;overflow:hidden">'
        f'<div style="position:absolute;left:0;top:0;width:{w}px;height:{h}px;{bgcss}"></div>'
        f'<img src="{svg_path.as_uri()}" style="position:absolute;left:{pad}px;top:{pad}px;width:{iw}px;height:{ih}px">',
        encoding="utf-8",
    )
    full = tmpd / "full.png"
    cmd = [ELECTRON, SHOOT, str(full), str(html), str(cw), str(ch)] + ([] if bg else ["transparent"])
    for _try in range(4):  # Electron's GPU process fails now and then (UnknownVizError): retry
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=90)
        if "SHOT" in r.stdout:
            break
    else:
        raise RuntimeError(r.stdout + r.stderr)
    im = Image.open(full).convert("RGBA")
    if im.size != (cw, ch):
        raise RuntimeError(f"capture size {im.size} != {(cw, ch)}")
    im = im.crop((0, 0, w, h))
    if bg is None and flat:  # single-colour art: make every visible pixel carry the fill colour (removes alpha 1-2 quantisation fringes)
        import numpy as np
        a = np.asarray(im).copy()
        op = a[a[..., 3] == 255][:, :3]
        if len(op):
            vals, cnt = np.unique(op, axis=0, return_counts=True)
            a[a[..., 3] > 0, :3] = vals[cnt.argmax()]
            im = Image.fromarray(a, "RGBA")
    im.save(out_png)
    return out_png
