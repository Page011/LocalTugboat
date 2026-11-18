"""Multi-size .ico files (PNG entries) from the rendered PNGs. Owner 2026-11-18: app icon AND tray are his artwork on
the sea tile, drawn from the vector at every size (no simplified pixel boat anywhere)."""
import os
import struct
from PIL import Image

B = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
R = os.environ.get("TUG_RENDERS", B + r"\renders")
OUT = os.path.join(B, "ico")
TRAY_SIZES = (16, 20, 24, 32, 40, 48)
# Largest first: a reader that takes the first entry gets 256 to shrink, never 16 to blow up.
APP_SIZES = (256, 64, 48, 40, 32, 24, 20, 16)


def write_ico(path, imgs):
    """imgs: list of RGBA PIL images, square, <= 256. Each stored as a PNG entry."""
    import io
    blobs = []
    for im in imgs:
        buf = io.BytesIO()
        im.save(buf, "PNG")
        blobs.append((im.width, im.height, buf.getvalue()))
    head = struct.pack("<HHH", 0, 1, len(blobs))
    off = 6 + 16 * len(blobs)
    ent, data = b"", b""
    for w, h, b in blobs:
        ent += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(b), off + len(data))
        data += b
    with open(path, "wb") as f:
        f.write(head + ent + data)


def build():
    os.makedirs(OUT, exist_ok=True)
    for state in ("synced", "waiting", "offline"):
        imgs = [Image.open(os.path.join(R, f"tray-{n}-{state}.png")).convert("RGBA") for n in TRAY_SIZES]
        write_ico(os.path.join(OUT, f"tray-{state}.ico"), imgs)
    app = [Image.open(os.path.join(R, f"tugboat-tile-{n}.png")).convert("RGBA") for n in APP_SIZES]
    write_ico(os.path.join(OUT, "tugboat-app.ico"), app)
    preview()
    print("ico built")



def preview():
    """renders/ico-sizes-sheet.png: every entry of tugboat-app.ico at 1:1 and 4x (nearest) on light and dark."""
    from PIL import ImageDraw
    sizes = (16, 20, 24, 32, 40, 48, 64, 256)
    ents = [Image.open(os.path.join(OUT, "tugboat-app.ico")).ico.getimage((s, s)).convert("RGBA") for s in sizes]
    W = 2 * 20 + sum(min(s, 64) * 4 + 12 for s in sizes[:7]) + 10
    sheet = Image.new("RGB", (max(W, 1400), 2 * 330), (255, 255, 255))
    d = ImageDraw.Draw(sheet)
    for j, bgc in enumerate(((243, 243, 243), (32, 32, 32))):
        y0 = j * 330
        d.rectangle([0, y0, sheet.width, y0 + 330], fill=bgc)
        d.text((8, y0 + 4), "tugboat-app.ico entries: 4x nearest (16 to 64) above, 1:1 below; 256 at 1:1 right", fill=(130, 130, 130))
        x = 10
        for s, im in zip(sizes[:7], ents[:7]):
            bg = Image.new("RGBA", im.size, bgc + (255,))
            bg.alpha_composite(im)
            sheet.paste(bg.convert("RGB").resize((s * 4, s * 4), Image.NEAREST), (x, y0 + 20))
            sheet.paste(bg.convert("RGB"), (x, y0 + 20 + 64 * 4 + 8))
            x += s * 4 + 12
        bg = Image.new("RGBA", ents[7].size, bgc + (255,))
        bg.alpha_composite(ents[7])
        sheet.paste(bg.convert("RGB").crop((0, 0, 256, 256)), (x + 20, y0 + 20))
    sheet.save(os.path.join(R, "ico-sizes-sheet.png"))


if __name__ == "__main__":
    build()
