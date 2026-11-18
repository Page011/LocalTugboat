"""Marching squares on Tugboat.png -> closed polylines (image px coords, pixel area 0..1254)."""
import json
import numpy as np
from PIL import Image
from scipy import ndimage

import os
from lib import source_png
SRC = source_png()
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "loops.json")
SIGMA = 0.6
DARK, LIGHT = 39.0, 254.0
LEVEL = (DARK + LIGHT) / 2


def load_gray():
    a = np.asarray(Image.open(SRC).convert("RGB")).astype(float)
    g = a.mean(axis=2)
    return g


def march(g, level):
    H, W = g.shape
    ins = g < level
    # corner flags
    tl = ins[:-1, :-1]
    tr = ins[:-1, 1:]
    br = ins[1:, 1:]
    bl = ins[1:, :-1]
    idx = tl * 1 + tr * 2 + br * 4 + bl * 8
    ys, xs = np.nonzero((idx > 0) & (idx < 15))
    segs = []

    def pt(edge, y, x):
        # edge in T,R,B,L for cell (y,x)
        if edge == "T":
            v0, v1 = g[y, x], g[y, x + 1]
            t = (level - v0) / (v1 - v0)
            return ("h", y, x), (x + t + 0.5, y + 0.5)
        if edge == "B":
            v0, v1 = g[y + 1, x], g[y + 1, x + 1]
            t = (level - v0) / (v1 - v0)
            return ("h", y + 1, x), (x + t + 0.5, y + 1.5)
        if edge == "L":
            v0, v1 = g[y, x], g[y + 1, x]
            t = (level - v0) / (v1 - v0)
            return ("v", y, x), (x + 0.5, y + t + 0.5)
        if edge == "R":
            v0, v1 = g[y, x + 1], g[y + 1, x + 1]
            t = (level - v0) / (v1 - v0)
            return ("v", y, x + 1), (x + 1.5, y + t + 0.5)

    table = {
        1: [("L", "T")], 2: [("T", "R")], 3: [("L", "R")], 4: [("R", "B")],
        6: [("T", "B")], 7: [("L", "B")], 8: [("L", "B")], 9: [("T", "B")],
        11: [("R", "B")], 12: [("L", "R")], 13: [("T", "R")], 14: [("L", "T")],
    }
    for y, x in zip(ys, xs):
        c = int(idx[y, x])
        if c in table:
            pairs = table[c]
        else:
            center = (g[y, x] + g[y, x + 1] + g[y + 1, x] + g[y + 1, x + 1]) / 4 < level
            if c == 5:
                pairs = [("L", "B"), ("T", "R")] if center else [("L", "T"), ("R", "B")]
            else:
                pairs = [("L", "T"), ("R", "B")] if center else [("T", "R"), ("L", "B")]
        for a, b in pairs:
            ka, pa = pt(a, y, x)
            kb, pb = pt(b, y, x)
            segs.append((ka, pa, kb, pb))
    adj = {}
    P = {}
    for ka, pa, kb, pb in segs:
        P[ka] = pa
        P[kb] = pb
        adj.setdefault(ka, []).append(kb)
        adj.setdefault(kb, []).append(ka)
    seen = set()
    loops = []
    for start in adj:
        if start in seen:
            continue
        loop = [start]
        seen.add(start)
        prev = None
        cur = start
        while True:
            nxt = [n for n in adj[cur] if n != prev]
            if prev is None:
                n = adj[cur][0]
            else:
                n = nxt[0] if nxt else None
            if n is None or n == start:
                break
            if n in seen:
                break
            loop.append(n)
            seen.add(n)
            prev, cur = cur, n
        loops.append(np.array([P[k] for k in loop]))
    return loops


def area(p):
    x, y = p[:, 0], p[:, 1]
    return 0.5 * np.sum(x * np.roll(y, -1) - np.roll(x, -1) * y)


if __name__ == "__main__":
    g = load_gray()
    g = ndimage.gaussian_filter(g, SIGMA)
    loops = march(g, LEVEL)
    out = []
    for l in loops:
        if len(l) < 20:
            continue
        a = area(l)
        out.append({"area": a, "n": len(l), "bbox": [float(l[:, 0].min()), float(l[:, 1].min()), float(l[:, 0].max()), float(l[:, 1].max())], "pts": l.tolist()})
        print(len(l), round(a, 1), [round(v, 1) for v in out[-1]["bbox"]])
    json.dump(out, open(OUT, "w"))
