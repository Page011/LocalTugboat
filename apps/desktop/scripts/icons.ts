// Regenerates the Windows/Tauri app icons from the logo kit in assets/brand.
//   bun scripts/icons.ts
//  - icon.ico   = assets/brand/ico/tugboat-app.ico copied as is: the owner's artwork on the sea tile, drawn from the
//                 vector at 256, 64, 48, 40, 32, 24, 20 and 16 px (owner 2026-11-18: no simplified pixel boat).
//                 The exe icon resource and the window/taskbar icon come from this file.
//  - 32x32.png, 128x128.png, 128x128@2x.png, icon.png = tugboat-tile-1024.svg rendered by `tauri icon`.
// `tauri icon` also writes Android and iOS sets and Store logos we do not ship, so it runs into a temp dir and
// only the files tauri.conf.json lists are copied over.
import { copyFileSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = join(import.meta.dir, "..");
const brand = join(root, "../../assets/brand");
const icons = join(root, "src-tauri/icons");
/** Frames the app icon must carry: the taskbar and Explorer ask for these at 100% to 250% scaling. */
const NEED = [16, 20, 24, 32, 40, 48, 64, 256];

/** Edge sizes of the frames in an .ico (0 in the directory means 256). */
function frameSizes(ico: Buffer): number[] {
  const count = ico.readUInt16LE(4);
  return Array.from({ length: count }, (_, i) => ico[6 + i * 16] || 256);
}

const ico = readFileSync(join(brand, "ico/tugboat-app.ico"));
const have = frameSizes(ico);
const missing = NEED.filter((n) => !have.includes(n));
if (missing.length) throw new Error(`assets/brand/ico/tugboat-app.ico lacks frames: ${missing.join(", ")}`);

const out = mkdtempSync(join(tmpdir(), "tugboat-icons-"));
try {
  const run = Bun.spawnSync(["bun", "run", "tauri", "icon", join(brand, "tugboat-tile-1024.svg"), "-o", out], { cwd: root, stdout: "inherit", stderr: "inherit" });
  if (run.exitCode !== 0) throw new Error("tauri icon failed");
  for (const f of ["32x32.png", "128x128.png", "128x128@2x.png", "icon.png"]) copyFileSync(join(out, f), join(icons, f));
  copyFileSync(join(brand, "ico/tugboat-app.ico"), join(icons, "icon.ico"));
  console.log(`icons updated (icon.ico frames: ${[...have].sort((a, b) => a - b).join(", ")})`);
} finally {
  rmSync(out, { recursive: true, force: true });
}
