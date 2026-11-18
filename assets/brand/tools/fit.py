"""Fit cubic Beziers (Schneider) to traced loops -> geom.json (source px frame)."""
import json
import sys
import numpy as np

import os
S = os.path.dirname(os.path.abspath(__file__))
TOL = float(sys.argv[1]) if len(sys.argv) > 1 else 0.45
CORNER_DEG = float(sys.argv[2]) if len(sys.argv) > 2 else 62.0
K = 5

loops = json.load(open(S + r"\loops.json"))


# ---------- helpers
def resample_closed(p, step=1.0):
    q = np.vstack([p, p[:1]])
    d = np.hypot(*np.diff(q, axis=0).T)
    s = np.r_[0, np.cumsum(d)]
    n = int(round(s[-1] / step))
    t = np.linspace(0, s[-1], n, endpoint=False)
    return np.c_[np.interp(t, s, q[:, 0]), np.interp(t, s, q[:, 1])]


def smooth_closed(p, w=2):
    out = np.zeros_like(p)
    for k in range(-w, w + 1):
        out += np.roll(p, k, axis=0)
    return out / (2 * w + 1)


def corners(p, deg, k=K):
    n = len(p)
    v1 = p - np.roll(p, k, axis=0)
    v2 = np.roll(p, -k, axis=0) - p
    a = np.degrees(np.arccos(np.clip((v1 * v2).sum(1) / (np.hypot(*v1.T) * np.hypot(*v2.T)), -1, 1)))
    cand = [i for i in range(n) if a[i] > deg and a[i] >= a[(np.arange(i - 8, i + 9)) % n].max()]
    return cand, a


def unit(v):
    n = np.hypot(*v)
    return v / n if n else v


# ---------- Schneider
def bez(ctrl, t):
    t = np.asarray(t)[:, None]
    return ((1 - t) ** 3) * ctrl[0] + 3 * ((1 - t) ** 2) * t * ctrl[1] + 3 * (1 - t) * t * t * ctrl[2] + t ** 3 * ctrl[3]


def bez1(ctrl, t):
    return 3 * (1 - t) ** 2 * (ctrl[1] - ctrl[0]) + 6 * (1 - t) * t * (ctrl[2] - ctrl[1]) + 3 * t * t * (ctrl[3] - ctrl[2])


def bez2(ctrl, t):
    return 6 * (1 - t) * (ctrl[2] - 2 * ctrl[1] + ctrl[0]) + 6 * t * (ctrl[3] - 2 * ctrl[2] + ctrl[1])


def chord_param(pts):
    d = np.r_[0, np.cumsum(np.hypot(*np.diff(pts, axis=0).T))]
    return d / d[-1]


def gen_bezier(pts, u, t1, t2):
    p0, p3 = pts[0], pts[-1]
    b0 = (1 - u) ** 3
    b1 = 3 * u * (1 - u) ** 2
    b2 = 3 * u * u * (1 - u)
    b3 = u ** 3
    A1 = t1[None, :] * b1[:, None]
    A2 = t2[None, :] * b2[:, None]
    C = np.array([[(A1 * A1).sum(), (A1 * A2).sum()], [(A1 * A2).sum(), (A2 * A2).sum()]])
    tmp = pts - (p0 * (b0 + b1)[:, None] + p3 * (b2 + b3)[:, None])
    X = np.array([(A1 * tmp).sum(), (A2 * tmp).sum()])
    det = C[0, 0] * C[1, 1] - C[0, 1] * C[1, 0]
    seg = np.hypot(*(p3 - p0))
    if abs(det) > 1e-12:
        a1 = (X[0] * C[1, 1] - X[1] * C[0, 1]) / det
        a2 = (C[0, 0] * X[1] - C[1, 0] * X[0]) / det
    else:
        a1 = a2 = 0
    eps = 1e-6 * seg
    if a1 < eps or a2 < eps:
        a1 = a2 = seg / 3
    return np.array([p0, p0 + t1 * a1, p3 + t2 * a2, p3])


def reparam(ctrl, pts, u):
    out = []
    for p, t in zip(pts, u):
        d = bez(ctrl, [t])[0] - p
        d1 = bez1(ctrl, t)
        d2 = bez2(ctrl, t)
        num = (d * d1).sum()
        den = (d1 * d1).sum() + (d * d2).sum()
        out.append(t - num / den if abs(den) > 1e-12 else t)
    return np.clip(np.array(out), 0, 1)


def max_err(ctrl, pts, u):
    d = ((bez(ctrl, u) - pts) ** 2).sum(1)
    i = int(d.argmax())
    return d[i], i


def fit_cubic(pts, t1, t2, err):
    if len(pts) == 2:
        dist = np.hypot(*(pts[1] - pts[0])) / 3
        return [np.array([pts[0], pts[0] + t1 * dist, pts[1] + t2 * dist, pts[1]])]
    u = chord_param(pts)
    c = gen_bezier(pts, u, t1, t2)
    e, split = max_err(c, pts, u)
    if e < err:
        return [c]
    if e < err * 16:
        for _ in range(25):
            u = reparam(c, pts, u)
            c = gen_bezier(pts, u, t1, t2)
            e, split = max_err(c, pts, u)
            if e < err:
                return [c]
    split = min(max(split, 1), len(pts) - 2)
    tc = unit(pts[split - 1] - pts[split + 1])
    return fit_cubic(pts[: split + 1], t1, tc, err) + fit_cubic(pts[split:], -tc, t2, err)


def fit_closed(p, tol, deg):
    p = resample_closed(p, 1.0)
    ps = smooth_closed(p, 2)
    cs, ang = corners(ps, deg)
    n = len(ps)
    if not cs:
        # no corners: break at index 0 with centred tangent (smooth join)
        t = unit(ps[1] - ps[-1])
        pts = np.vstack([ps, ps[:1]])
        return fit_cubic(pts, t, -t, tol * tol), []
    # rotate so first corner is at 0
    c0 = cs[0]
    ps = np.roll(ps, -c0, axis=0)
    cs = [(c - c0) % n for c in cs]
    cs.append(n)
    segs = []
    for a, b in zip(cs[:-1], cs[1:]):
        pts = ps[a : b + 1] if b < n else np.vstack([ps[a:], ps[:1]])
        k = min(6, len(pts) - 1)
        t1 = unit(pts[k] - pts[0])
        t2 = unit(pts[-1 - k] - pts[-1])
        segs += fit_cubic(pts, t1, t2, tol * tol)
    return segs, [c for c in cs[:-1]]


def dev(segs, loop):
    # max distance from loop points to the fitted curve (sampled)
    s = np.vstack([bez(c, np.linspace(0, 1, 400)) for c in segs])
    d = np.sqrt(((loop[:, None, :] - s[None, ::1, :]) ** 2).sum(2)).min(1)
    return d.max(), d.mean()


# ---------- circles
def fit_circle(p):
    x, y = p[:, 0], p[:, 1]
    A = np.c_[2 * x, 2 * y, np.ones(len(x))]
    c, *_ = np.linalg.lstsq(A, x ** 2 + y ** 2, rcond=None)
    r = np.sqrt(c[2] + c[0] ** 2 + c[1] ** 2)
    return float(c[0]), float(c[1]), float(r)


by_n = {l["n"]: np.array(l["pts"]) for l in loops}
cap, cabin, window, hull, wave = by_n[584], by_n[800], by_n[382], by_n[2032], by_n[2404]
circles = [fit_circle(by_n[n]) for n in (200, 198, 200 + 0) if n in by_n]
# the two 200-point loops collide in the dict; gather by bbox instead
circ_loops = [np.array(l["pts"]) for l in loops if 180 < l["n"] < 220]
circles = sorted([fit_circle(c) for c in circ_loops], key=lambda c: c[0])
R = float(np.mean([c[2] for c in circles]))

# ---------- leftmost porthole: fit from the hull arc, bridge the notch
zone = np.nonzero((hull[:, 0] > 470) & (hull[:, 0] < 525) & (hull[:, 1] > 728) & (hull[:, 1] < 770))[0]
arc = hull[zone]
cx0, cy0, r0 = fit_circle(arc)
for _ in range(3):  # refit using only points close to the circle
    d = np.abs(np.hypot(hull[:, 0] - cx0, hull[:, 1] - cy0) - r0)
    near = np.nonzero((d < 1.2) & (hull[:, 1] < 777) & (np.hypot(hull[:, 0] - cx0, hull[:, 1] - cy0) < r0 + 2))[0]
    near = near[(hull[near, 0] > 460) & (hull[near, 0] < 535)]
    cx0, cy0, r0 = fit_circle(hull[near])
d = np.abs(np.hypot(hull[:, 0] - cx0, hull[:, 1] - cy0) - R)
cand = np.nonzero((d < 1.0) & (hull[:, 0] > 460) & (hull[:, 0] < 535) & (hull[:, 1] < 777))[0]
i_in, i_out = int(cand.min()), int(cand.max())
print("leftmost porthole fit", round(cx0, 2), round(cy0, 2), round(r0, 2), "in/out idx", i_in, i_out, hull[i_in], hull[i_out])
P0, P3 = hull[i_in - 1], hull[i_out + 1]
T0 = unit(hull[i_in - 1] - hull[i_in - 13])
T3 = unit(hull[i_out + 13] - hull[i_out + 1])
L = np.hypot(*(P3 - P0)) / 3
br = bez(np.array([P0, P0 + T0 * L, P3 - T3 * L, P3]), np.linspace(0, 1, int(np.hypot(*(P3 - P0))) + 2))
hull_fixed = np.vstack([hull[: i_in - 1], br, hull[i_out + 2 :]])

# web (gap) between each full porthole and the hull bottom edge, measured on the fixed hull
def web(cx, cy):
    below = hull_fixed[(hull_fixed[:, 1] > cy) & (np.abs(hull_fixed[:, 0] - cx) < 60)]
    return float(np.hypot(below[:, 0] - cx, below[:, 1] - cy).min() - R)


def web_top(cx, cy):
    above = hull_fixed[(hull_fixed[:, 1] < cy) & (np.abs(hull_fixed[:, 0] - cx) < 60)]
    return float(np.hypot(above[:, 0] - cx, above[:, 1] - cy).min() - R)


print("R", round(R, 3))
for c in circles:
    print("porthole", [round(v, 2) for v in c], "web below", round(web(c[0], c[1]), 2), "web above", round(web_top(c[0], c[1]), 2))
print("leftmost original: web below", round(web(cx0, cy0), 2), "web above", round(web_top(cx0, cy0), 2))
# OWNER DECISION 2026-11-18 (Nigel): the leftmost porthole cut open by the hull's bottom edge is a deliberate,
# signature part of the design ("no one else has that"). Keep it exactly as drawn in the PNG: the hull outline
# keeps its traced notch, nothing is bridged or moved, and only the other three portholes are separate circles.
# Never "fix" this.
KEEP_ORIGINAL_PORTHOLE = True
if KEEP_ORIGINAL_PORTHOLE:
    hull_fixed = hull
    allc = list(circles)
    print("leftmost porthole kept as drawn (notch in the hull outline), centre", round(cx0, 2), round(cy0, 2))
    geom = {"R": R, "circles": [list(c) for c in allc], "leftmost_orig": [cx0, cy0, r0], "move_up": 0.0, "move_right": 0.0}
else:
    MOVE_UP, MOVE_RIGHT = 20.0, 10.0
    y = cy0 - MOVE_UP
    cx0n = cx0 + MOVE_RIGHT
    print("leftmost moved up", MOVE_UP, "right", MOVE_RIGHT, "-> centre", round(cx0n, 2), round(y, 2),
          "web below", round(web(cx0n, y), 2), "web above", round(web_top(cx0n, y), 2))
    print("min web of the other three", round(min(web(c[0], c[1]) for c in circles), 2))
    leftmost = (cx0n, y, R)
    allc = sorted([leftmost] + circles, key=lambda c: c[0])
    geom = {"R": R, "circles": [list(c) for c in allc], "leftmost_orig": [cx0, cy0, r0], "move_up": cy0 - y, "move_right": MOVE_RIGHT}

# ---------- fit paths
for name, loop in (("cap", cap), ("cabin", cabin), ("window", window), ("hull", hull_fixed), ("wave", wave)):
    segs, cs = fit_closed(loop, TOL, CORNER_DEG)
    m, mean = dev(segs, resample_closed(loop, 1.0))
    print(name, "segments", len(segs), "corners", len(cs), "max dev px", round(m, 3), "mean", round(mean, 3))
    geom[name] = [[[round(float(v), 2) for v in pt] for pt in c] for c in segs]
json.dump(geom, open(S + r"\geom.json", "w"))
