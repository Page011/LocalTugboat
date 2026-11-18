import os
import json
import numpy as np
from PIL import Image, ImageDraw
from scipy import ndimage
from lib import S

B = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
src = np.asarray(Image.open(r"C:\Users\Nigel\Downloads\Tugboat.png").convert("RGB"))
cv = np.asarray(Image.open(S + r"\logo_on_canvas.png").convert("L"))
srcg = src.astype(float).mean(axis=2)
A = srcg < 146
Bm = cv < 146
X0, Y0, X1, Y1 = 155, 475, 1099, 877  # crop with margin
P = 0

# side by side, same scale (1:1)
w, h = X1 - X0, Y1 - Y0
side = Image.new("RGB", (w * 2 + 30, h + 50), "white")
side.paste(Image.fromarray(src).crop((X0, Y0, X1, Y1)), (0, 50))
# right panel: the real colour render (SVG fill on white) placed on the PNG's grid; the greyscale canvas is only for the mask maths
rgb = Image.new("RGB", (1254, 1254), "white")
rgb.paste(Image.open(S + r"\logo_1to1.png").convert("RGB"), (175, 495))
side.paste(rgb.crop((X0, Y0, X1, Y1)), (w + 30, 50))
d = ImageDraw.Draw(side)
d.text((10, 15), "Tugboat.png (original)", fill=(0, 0, 0))
d.text((w + 40, 15), "tugboat-logo.svg (rendered 1:1)", fill=(0, 0, 0))
d.line([(w + 15, 0), (w + 15, h + 50)], fill=(180, 180, 180))
side.save(B + r"\renders\compare-side-by-side.png")

# difference overlay: grey = both dark, red = only in original, blue = only in SVG
ov = np.full(A.shape + (3,), 255, np.uint8)
ov[A & Bm] = (190, 190, 190)
ov[A & ~Bm] = (230, 30, 30)
ov[~A & Bm] = (30, 90, 230)
# approved-change zone box
im = Image.fromarray(ov).crop((X0, Y0, X1, Y1))
dd = ImageDraw.Draw(im)
dd.rectangle([455 - X0, 700 - Y0, 540 - X0, 795 - Y0], outline=(0, 160, 0))
dd.text((10, 8), "grey = both, red = original only, blue = SVG only, green box = approved porthole change", fill=(0, 0, 0))
im = im.resize((w * 2, h * 2), Image.NEAREST)
im.save(B + r"\renders\compare-diff-overlay.png")
print("saved")
