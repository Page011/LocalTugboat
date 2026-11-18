"""Checks: the app .ico and the three tray .ico files carry every size, the tray 'synced' icon IS the app icon at each
size, the three tray states differ at every size, and every entry is square, opaque in the middle, clear in the corners."""
import os
import numpy as np
from PIL import Image

B = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICO = os.path.join(B, "ico")
TRAY_SIZES = (16, 20, 24, 32, 40, 48)
APP_SIZES = (16, 20, 24, 32, 40, 48, 64, 256)
bad = 0


def entry(name, n):
    return np.asarray(Image.open(os.path.join(ICO, name)).ico.getimage((n, n)).convert("RGBA")).astype(int)


def fail(msg):
    global bad
    bad += 1
    print("FAIL", msg)


for f, sizes in [("tugboat-app.ico", APP_SIZES)] + [(f"tray-{s}.ico", TRAY_SIZES) for s in ("synced", "waiting", "offline")]:
    have = sorted(Image.open(os.path.join(ICO, f)).info["sizes"])
    if sorted((n, n) for n in sizes) != have:
        fail(f"{f} sizes {have}")
    for n in sizes:
        a = entry(f, n)
        if a.shape != (n, n, 4):
            fail(f"{f} {n}: shape {a.shape}")
        if a[n // 2, n // 2, 3] != 255 or a[0, 0, 3] > 64:
            fail(f"{f} {n}: centre alpha {a[n // 2, n // 2, 3]}, corner alpha {a[0, 0, 3]}")
for n in TRAY_SIZES:
    if not (entry("tray-synced.ico", n) == entry("tugboat-app.ico", n)).all():
        fail(f"tray synced {n} != app icon {n}")
    s, w, o = (entry(f"tray-{x}.ico", n) for x in ("synced", "waiting", "offline"))
    tr = (slice(0, n // 3), slice(n - n // 3, n))  # top-right third: where the waiting dot is
    if np.abs(w[tr] - s[tr]).sum() < 255 * 3 * 2:
        fail(f"{n}: waiting dot not visible top right")
    if np.abs(o - s).sum() < 255 * 3 * n:
        fail(f"{n}: offline not different enough from synced")
print("icons checked, bad", bad)
raise SystemExit(1 if bad else 0)
