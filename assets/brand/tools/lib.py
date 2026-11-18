"""Geometry helpers: geom.json (source px frame) -> SVG path data under a uniform scale + translate."""
import json
import numpy as np

import os
S = os.path.dirname(os.path.abspath(__file__))
NAVY = "#1A2733"


def source_png():
    """The original Tugboat.png the logo was traced from; its path is in the TUG_SOURCE environment variable."""
    p = os.environ.get("TUG_SOURCE")
    if not p:
        raise SystemExit("TUG_SOURCE is not set: set it to the path of Tugboat.png")
    if not os.path.isfile(p):
        raise SystemExit("Source logo PNG not found at %s: set TUG_SOURCE to the path of Tugboat.png" % p)
    return p


def load():
    return json.load(open(S + r"\geom.json"))


def fmt(v):
    s = f"{v:.2f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "") else s


class Xf:
    def __init__(self, s=1.0, ox=0.0, oy=0.0, tx=0.0, ty=0.0):
        # out = (p - o) * s + t
        self.s, self.ox, self.oy, self.tx, self.ty = s, ox, oy, tx, ty

    def pt(self, p):
        return ((p[0] - self.ox) * self.s + self.tx, (p[1] - self.oy) * self.s + self.ty)

    def pts(self, p):
        return " ".join(f"{fmt(v)}" for v in self.pt(p))


def bez_path(segs, xf):
    out = ["M" + xf.pts(segs[0][0])]
    for c in segs:
        out.append("C" + " ".join(xf.pts(p) for p in c[1:]))
    out.append("Z")
    return "\n".join(out)


def circle_path(c, xf):
    cx, cy, r = c
    x, y = xf.pt((cx, cy))
    r = r * xf.s
    return f"M{fmt(x - r)} {fmt(y)}A{fmt(r)} {fmt(r)} 0 1 0 {fmt(x + r)} {fmt(y)}A{fmt(r)} {fmt(r)} 0 1 0 {fmt(x - r)} {fmt(y)}Z"


def bbox(g):
    pts = []
    for k in ("cap", "cabin", "window", "hull", "wave"):
        for c in g[k]:
            c = np.array(c)
            t = np.linspace(0, 1, 50)[:, None]
            pts.append((1 - t) ** 3 * c[0] + 3 * (1 - t) ** 2 * t * c[1] + 3 * (1 - t) * t * t * c[2] + t ** 3 * c[3])
    for cx, cy, r in g["circles"]:
        pts.append(np.array([[cx - r, cy - r], [cx + r, cy + r]]))
    p = np.vstack(pts)
    return p[:, 0].min(), p[:, 1].min(), p[:, 0].max(), p[:, 1].max()


def parts(g, xf):
    """Ordered list of (name, path data). Each part is its own evenodd path."""
    hull = bez_path(g["hull"], xf) + "\n" + "\n".join(circle_path(c, xf) for c in g["circles"])
    cabin = bez_path(g["cabin"], xf) + "\n" + bez_path(g["window"], xf)
    return [
        ("cap", bez_path(g["cap"], xf)),
        ("cabin", cabin),
        ("hull", hull),
        ("wave", bez_path(g["wave"], xf)),
    ]


def svg_doc(vb_w, vb_h, body, fill=NAVY, extra="", width=None, height=None, title=None):
    wh = f' width="{width}" height="{height}"' if width else ""
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {fmt(vb_w)} {fmt(vb_h)}"{wh} fill="{fill}"{extra}>\n'
        + (f"<title>{title}</title>\n" if title else "")
        + body
        + "</svg>\n"
    )


def paths_svg(g, xf, titles=True):
    out = ""
    for name, d in parts(g, xf):
        out += f'<path id="{name}" fill-rule="evenodd" d="{d}"/>\n'
    return out
