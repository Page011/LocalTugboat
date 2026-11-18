import os
import sys
import math
import numpy as np
from PIL import Image
from scipy import ndimage
from lib import *
from render import render_svg

B = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OX, OY = 175, 495
VBW, VBH = 904, 362

g = load()
x0, y0, x1, y1 = bbox(g)
print("bbox", round(x0, 2), round(y0, 2), round(x1, 2), round(y1, 2), "-> needs", math.ceil(x1 - OX), math.ceil(y1 - OY))
xf = Xf(1.0, OX, OY, 0, 0)
svg = svg_doc(VBW, VBH, paths_svg(g, xf))
open(B + r"\tugboat-logo.svg", "w").write(svg)

# render at 1:1 on white, composite into the 1254 canvas
tmp = S + r"\logo_1to1.png"
render_svg(B + r"\tugboat-logo.svg", tmp, VBW, VBH, bg="#ffffff")
r = np.asarray(Image.open(tmp).convert("RGB")).astype(float).mean(axis=2)
canvas = np.full((1254, 1254), 254.0)
canvas[OY : OY + VBH, OX : OX + VBW] = r
src = np.asarray(Image.open(r"C:\Users\Nigel\Downloads\Tugboat.png").convert("RGB")).astype(float).mean(axis=2)
LEVEL = 146
A = src < LEVEL
Bm = canvas < LEVEL
Image.fromarray(canvas.astype(np.uint8)).save(S + r"\logo_on_canvas.png")

excl = np.zeros_like(A)
excl[700:795, 455:540] = True
st = np.ones((3, 3), bool)


def metrics(mask_ok):
    a, b = A & mask_ok, Bm & mask_ok
    diff = (A ^ Bm) & mask_ok
    union = (a | b).sum()
    far = ((A & ~ndimage.binary_dilation(Bm, st)) | (Bm & ~ndimage.binary_dilation(A, st))) & mask_ok
    far2 = ((A & ~ndimage.binary_dilation(Bm, st, iterations=2)) | (Bm & ~ndimage.binary_dilation(A, st, iterations=2))) & mask_ok
    return dict(
        dark_px_src=int(a.sum()),
        diff_px=int(diff.sum()),
        diff_pct_of_dark=round(100 * diff.sum() / a.sum(), 3),
        iou=round(1 - diff.sum() / union, 5),
        beyond_1px=int(far.sum()),
        beyond_2px=int(far2.sum()),
        mean_abs_gray=round(float(np.abs(src - canvas)[mask_ok].mean()), 3),
    )


print("ALL        ", metrics(np.ones_like(A)))
print("EXCL PORTH ", metrics(~excl))
